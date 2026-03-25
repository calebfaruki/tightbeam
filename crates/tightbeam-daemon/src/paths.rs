use std::path::PathBuf;

#[derive(Debug)]
pub struct DaemonPaths {
    pub config_dir: PathBuf,
    pub sockets_dir: PathBuf,
    pub logs_dir: PathBuf,
}
