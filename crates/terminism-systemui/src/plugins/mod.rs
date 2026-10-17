mod account;
pub(crate) mod file_picker;
mod instances;
mod pages;
mod styles_manager;
mod ui_manager;

pub use account::Account;
pub use instances::Instances;
pub use pages::SystemPages;
pub use styles_manager::{StylesManager, load_theme};
use terminism_kernel::manifest::native as manifest;
pub use ui_manager::UIManager;
