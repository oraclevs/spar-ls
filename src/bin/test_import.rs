use spar::ast::{FieldValue, ObjectItem, TopLevelItem};
use spar::lexer::Lexer;
use spar::parser::Parser;

fn check(label: &str, src: &str) {
    match Lexer::new(src).tokenize() {
        Err(e) => {
            println!("{}: LEX ERROR: {:?}", label, e);
        }
        Ok(tokens) => match Parser::new(tokens).parse() {
            Err(e) => {
                println!("{}: PARSE ERROR: {:?}", label, e);
            }
            Ok(program) => {
                for item in &program.items {
                    if let TopLevelItem::Struct(s) = item {
                        if s.name == "Database" {
                            for si in &s.items {
                                if let ObjectItem::Field(fd) = si {
                                    if fd.name == "replica" {
                                        if let Some(FieldValue::Object(sub)) = &fd.value {
                                            for si in sub {
                                                let ObjectItem::Field(sf) = si else { continue };
                                                if sf.name == "asker" {
                                                    println!(
                                                        "{}: asker.value = {:?}",
                                                        label,
                                                        sf.value.as_ref().map(|v| match v {
                                                            FieldValue::Expr(_) =>
                                                                "Expr".to_string(),
                                                            FieldValue::Object(n) =>
                                                                format!("Nested({})", n.len()),
                                                        })
                                                    );
                                                    return;
                                                }
                                            }
                                            println!(
                                                "{}: asker NOT FOUND in replica sub-fields",
                                                label
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                println!("{}: Database struct or replica field not found", label);
            }
        },
    }
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: test_import <file.spar>");
        std::process::exit(2);
    });
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {path}: {error}"));
    // Get just the Database struct block
    let db_start = src.find("struct Database").unwrap();
    let db_end = src[db_start..].find("\n};").unwrap() + db_start + 3;
    let db_block = &src[db_start..db_end];
    println!("Block:\n{}\n", db_block);
    check("db-block-only", db_block);
}
