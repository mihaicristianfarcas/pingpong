//! What Pong keeps in its data folder, shared by the host (`pong`) and its
//! command line (`pongctl`): where the folder is and the host's settings
//! (`config`), the paired clients (`clients`), and who may read the folder
//! (`private`).

pub mod clients;
pub mod config;
pub mod private;
