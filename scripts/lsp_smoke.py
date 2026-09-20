#!/usr/bin/env python3
"""Stdio smoke test for spar-ls. Usage: python3 scripts/lsp_smoke.py [path-to-spar-ls]

Opens a valid file, edits it into an incomplete state (how real typing behaves),
and checks the intelligence a client receives. Editor independent: plain LSP
framing only, so it doubles as a regression check for Neovim/Zed/JetBrains.
"""
import json, os, subprocess, sys, tempfile, time

SERVER = sys.argv[1] if len(sys.argv) > 1 else "spar-ls"


class Client:
    def __init__(self, root):
        self.proc = subprocess.Popen([SERVER, "--stdio", "--stdio"],  # VS Code passes --stdio and the client appends its own
                                     stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.root = root
        self.next_id = 1

    def send(self, obj):
        body = json.dumps(obj).encode()
        self.proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        self.proc.stdin.flush()

    def recv(self):
        header = b""
        while not header.endswith(b"\r\n\r\n"):
            chunk = self.proc.stdout.read(1)
            if not chunk:
                raise RuntimeError("server closed the pipe")
            header += chunk
        length = int([l for l in header.decode().split("\r\n") if l.lower().startswith("content-length")][0].split(":")[1])
        return json.loads(self.proc.stdout.read(length))

    def request(self, method, params):
        request_id = self.next_id
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        while True:
            message = self.recv()
            if message.get("id") == request_id and "method" not in message:
                return message.get("result")

    def notify(self, method, params):
        self.send({"jsonrpc": "2.0", "method": method, "params": params})

    def start(self):
        caps = {"textDocument": {"completion": {"completionItem": {"snippetSupport": True}},
                                 "documentSymbol": {"hierarchicalDocumentSymbolSupport": True}}}
        self.request("initialize", {"processId": None, "rootUri": "file://" + self.root, "capabilities": caps})
        self.notify("initialized", {})


def marker(text):
    index = text.index("|")
    clean = text.replace("|", "", 1)
    line = clean[:index].count("\n")
    col = index - (clean.rfind("\n", 0, index) + 1)
    return clean, {"line": line, "character": col}


def items_of(result):
    return result if isinstance(result, list) else (result or {}).get("items", [])


def labels(result):
    return [item["label"] for item in items_of(result)]


def ranked(result):
    return [i["label"] for i in sorted(items_of(result), key=lambda i: i.get("sortText") or i["label"])]


failures = []


def check(name, condition, detail=""):
    print(("PASS " if condition else "FAIL ") + name + ("" if condition else f"  {detail}"))
    if not condition:
        failures.append(name)


with tempfile.TemporaryDirectory() as root:
    path = os.path.join(root, "main.spar")
    uri = "file://" + path
    client = Client(root)
    client.start()

    base = ('import pkg { writeText } from "std/fs";\n'
            'function f(a: int, b: str = "x") -> int { return a; };\n'
            'var gv: int = 1;\n')
    open(path, "w").write(base)
    client.notify("textDocument/didOpen", {"textDocument": {"uri": uri, "languageId": "spar", "version": 1, "text": base}})
    time.sleep(1.5)
    version = [1]

    def edit(text):
        version[0] += 1
        client.notify("textDocument/didChange", {"textDocument": {"uri": uri, "version": version[0]},
                                                 "contentChanges": [{"text": text}]})
        time.sleep(0.8)

    def complete_at(tail):
        text, position = marker(base + tail)
        edit(text)
        return client.request("textDocument/completion", {"textDocument": {"uri": uri}, "position": position})

    result = complete_at("function g(p: int) -> int { var loc: int = 2; return |; };\n")
    found = ranked(result)
    check("locals and params complete", "loc" in found and "p" in found, found[:12])
    check("file-level symbols complete", "f" in found and "gv" in found, found[:12])
    check("locals rank before keywords", found.index("loc") < found.index("if"), found[:20])

    result = complete_at("var y: int = f(|);\n")
    check("named params in declared order", ranked(result)[:2] == ["a:", "b:"], ranked(result))

    result = complete_at("var y: int = f(a: |);\n")
    got = ranked(result)
    check("value position offers values not param names", "b:" not in got and "gv" in got, got[:10])
    check("value position is not a type-annotation list", got[:3] != ["int", "float", "str"], got[:10])

    result = complete_at("import pkg { | ")
    write_text = [i for i in items_of(result) if i["label"] == "writeText"]
    check("import discovery lists std exports", bool(write_text), labels(result)[:10])
    check("import discovery adds from clause",
          bool(write_text) and 'from "std/fs"' in (write_text[0].get("insertText") or ""), write_text[:1])

    edit(base + 'var s: str = "error var int";\n// function in comment\n')
    tokens = client.request("textDocument/semanticTokens/full", {"textDocument": {"uri": uri}})
    check("semantic tokens returned", tokens is not None and len(tokens["data"]) > 0)

    edit(base + "var z: int = 1\nvar w: int = 2;\n")
    tokens = client.request("textDocument/semanticTokens/full", {"textDocument": {"uri": uri}})
    check("semantic tokens survive syntax errors", tokens is not None and len(tokens["data"]) > 0)

    def symbols_valid(symbols):
        def le(a, b):
            return (a["line"], a["character"]) <= (b["line"], b["character"])
        for symbol in symbols:
            if not symbol["name"]:
                return False
            if not (le(symbol["range"]["start"], symbol["selectionRange"]["start"])
                    and le(symbol["selectionRange"]["end"], symbol["range"]["end"])):
                return False
            if not symbols_valid(symbol.get("children") or []):
                return False
        return True

    outline = ('struct Config {\n    a: int = 1;\n};\n'
               'function startup() -> shell {\n    return shell { pwd; };\n};\n')
    edit(base + outline)
    symbols = client.request("textDocument/documentSymbol", {"textDocument": {"uri": uri}})
    check("document symbols have selectionRange inside range (VS Code rejects otherwise)",
          isinstance(symbols, list) and len(symbols) > 0 and symbols_valid(symbols), symbols)

    client.proc.terminate()

print("\n%d failure(s)" % len(failures))
sys.exit(1 if failures else 0)
