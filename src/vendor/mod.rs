//! Everything that names a specific engine, hub, file format or device.
//!
//! The execution engine (`config`, `command`, `prompt`, `backend`,
//! `output`, `runtime::{docker,process}`) knows protocols and processes,
//! never a vendor. Everything that names an engine, a hub, a file format of
//! one vendor or a device lives here instead. A built-in may depend on
//! `vendor::*`; nothing in the engine may — `make lint` greps for it.
//!
//! - [`openvino`]: the `graph.pbtxt`/`plugin_config` shape one NPU-compiled
//!   OVMS export uses, and the registry of architectures `optimum-intel`
//!   exports to it.
//! - [`huggingface`]: the Hub's search API and a model export's
//!   `config.json` shape.
//! - [`llmfit`]: the optional `llmfit` CLI.

pub mod huggingface;
pub mod llmfit;
pub mod openvino;
