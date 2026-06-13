//! Concrete parser implementations for each supported format.

pub mod html;
pub mod markdown;
pub mod pdf;
pub mod plaintext;

pub use html::HtmlParser;
pub use markdown::MarkdownParser;
pub use pdf::PdfParser;
pub use plaintext::PlaintextParser;
