//! Lexing and parsing.

pub mod ast;
pub mod highlight;
pub mod lexer;
pub mod parser;

pub use parser::parse_module;
