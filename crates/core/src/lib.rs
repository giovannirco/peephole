//! Peephole core: the contracts every plugin, renderer and the hub share.
//!
//! A [`Plugin`] polls a source and returns values; the [`Store`] keeps the latest
//! value per key with its age; a [`Page`] describes cards and widgets bound to keys;
//! the [`Registry`] tracks the devices that fetch frames.

pub mod config;
pub mod device;
pub mod page;
pub mod plugin;
pub mod scheduler;
pub mod value;

pub use config::Config;
pub use device::{Device, Registry};
pub use page::{Card, Color, Fmt, Page, Thresholds, Widget};
pub use plugin::{Event, Plugin};
pub use value::{Stamped, Store, Value};
