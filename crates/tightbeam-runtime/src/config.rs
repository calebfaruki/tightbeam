use std::path::PathBuf;

pub(crate) const AGENT_DIR: &str = "/etc/agent";
use tightbeam_protocol::SOCKET_PATH;

pub(crate) struct RuntimeConfig {
    pub tools: Vec<String>,
    pub socket_path: PathBuf,
    pub max_iterations: u32,
    pub max_output_chars: usize,
}

impl RuntimeConfig {
    pub(crate) fn from_args(args: &[String]) -> Result<Self, String> {
        let mut tools = None;
        let mut max_iterations = 100u32;
        let mut max_output_chars = 30_000usize;

        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--tools" => {
                    i += 1;
                    let val = args.get(i).ok_or("--tools requires a value")?;
                    tools = Some(
                        val.split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                    );
                }
                "--max-iterations" => {
                    i += 1;
                    let val = args.get(i).ok_or("--max-iterations requires a value")?;
                    max_iterations = val
                        .parse()
                        .map_err(|_| format!("invalid --max-iterations: {val}"))?;
                }
                "--max-output-chars" => {
                    i += 1;
                    let val = args.get(i).ok_or("--max-output-chars requires a value")?;
                    max_output_chars = val
                        .parse()
                        .map_err(|_| format!("invalid --max-output-chars: {val}"))?;
                }
                other => return Err(format!("unknown flag: {other}")),
            }
            i += 1;
        }

        Ok(Self {
            tools: tools.ok_or("--tools is required")?,
            socket_path: PathBuf::from(SOCKET_PATH),
            max_iterations,
            max_output_chars,
        })
    }
}

#[cfg(test)]
mod config_parsing {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(|s| s.to_string()).collect()
    }

    #[test]
    fn full_args_parse() {
        let a = args("--tools bash,read_file --max-iterations 50 --max-output-chars 10000");
        let config = RuntimeConfig::from_args(&a).unwrap();
        assert_eq!(config.tools, vec!["bash", "read_file"]);
        assert_eq!(config.socket_path, PathBuf::from(SOCKET_PATH));
        assert_eq!(config.max_iterations, 50);
        assert_eq!(config.max_output_chars, 10000);
    }

    #[test]
    fn defaults_for_optional_flags() {
        let a = args("--tools bash");
        let config = RuntimeConfig::from_args(&a).unwrap();
        assert_eq!(config.socket_path, PathBuf::from(SOCKET_PATH));
        assert_eq!(config.max_iterations, 100);
        assert_eq!(config.max_output_chars, 30000);
    }

    #[test]
    fn missing_tools_errors() {
        assert!(RuntimeConfig::from_args(&[]).is_err());
    }

    #[test]
    fn unknown_flag_errors() {
        let a = args("--tools bash --bogus");
        assert!(RuntimeConfig::from_args(&a).is_err());
    }

    #[test]
    fn tools_split_by_comma() {
        let a = args("--tools bash,read_file,write_file,list_directory");
        let config = RuntimeConfig::from_args(&a).unwrap();
        assert_eq!(
            config.tools,
            vec!["bash", "read_file", "write_file", "list_directory"]
        );
    }

    #[test]
    fn double_comma_in_tools_filters_empty() {
        let a = vec!["--tools".to_string(), "bash,,read_file".to_string()];
        let config = RuntimeConfig::from_args(&a).unwrap();
        assert_eq!(config.tools, vec!["bash", "read_file"]);
    }

    #[test]
    fn invalid_max_iterations_errors() {
        let a = args("--tools bash --max-iterations abc");
        assert!(RuntimeConfig::from_args(&a).is_err());
    }

    #[test]
    fn negative_max_iterations_errors() {
        let a = args("--tools bash --max-iterations -1");
        assert!(RuntimeConfig::from_args(&a).is_err());
    }
}
