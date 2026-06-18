//! Concrete parser implementations for each supported format.

pub mod epub;
pub mod html;
pub mod markdown;
pub mod office;
pub mod pdf;
pub mod plaintext;

pub use epub::EpubParser;
pub use html::HtmlParser;
pub use markdown::MarkdownParser;
pub use office::OfficeParser;
pub use pdf::PdfParser;
pub use plaintext::PlaintextParser;
