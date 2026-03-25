use std::path::{Path, PathBuf};

pub struct DaemonPaths {
    pub config_dir: PathBuf,
    pub sockets_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub pid_path: PathBuf,
}

impl DaemonPaths {
    pub fn from_home(home: &Path) -> Self {
        let config_dir = home.join(".config").join("tightbeam");
        let logs_dir = home.join(".local/share/tightbeam").join("logs");
        let pid_path = config_dir.join("tightbeam.pid");
        let sockets_dir = config_dir.join("sockets");
        Self {
            config_dir,
            sockets_dir,
            logs_dir,
            pid_path,
        }
    }

    pub fn detect() -> Self {
        let home = std::env::var("HOME").expect("HOME not set");
        Self::from_home(Path::new(&home))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_home_builds_expected_paths() {
        let paths = DaemonPaths::from_home(Path::new("/home/test"));
        assert_eq!(
            paths.config_dir,
            PathBuf::from("/home/test/.config/tightbeam")
        );
        assert_eq!(
            paths.sockets_dir,
            PathBuf::from("/home/test/.config/tightbeam/sockets")
        );
        assert_eq!(
            paths.logs_dir,
            PathBuf::from("/home/test/.local/share/tightbeam/logs")
        );
        assert_eq!(
            paths.pid_path,
            PathBuf::from("/home/test/.config/tightbeam/tightbeam.pid")
        );
    }
}
