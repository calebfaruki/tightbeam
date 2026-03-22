use std::path::PathBuf;

pub(crate) struct RuntimeConfig {
    pub system_prompt_path: PathBuf,
    pub tools: Vec<String>,
    pub socket_path: PathBuf,
    pub max_iterations: u32,
    pub max_output_chars: usize,
}

impl RuntimeConfig {
    pub(crate) fn from_args(args: &[String]) -> Result<Self, String> {
        let mut system_prompt_path = None;
        let mut tools = None;
        let mut socket_path = None;
        let mut max_iterations = 100u32;
        let mut max_output_chars = 30_000usize;

        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--system-prompt" => {
                    i += 1;
                    system_prompt_path = Some(PathBuf::from(
                        args.get(i).ok_or("--system-prompt requires a value")?,
                    ));
                }
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
                "--socket" => {
                    i += 1;
                    socket_path = Some(PathBuf::from(
                        args.get(i).ok_or("--socket requires a value")?,
                    ));
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
            system_prompt_path: system_prompt_path.ok_or("--system-prompt is required")?,
            tools: tools.ok_or("--tools is required")?,
            socket_path: socket_path.ok_or("--socket is required")?,
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
        let a = args("--system-prompt /etc/agent/prompt.md --tools bash,read_file --socket /run/tb.sock --max-iterations 50 --max-output-chars 10000");
        let config = RuntimeConfig::from_args(&a).unwrap();
        assert_eq!(
            config.system_prompt_path,
            PathBuf::from("/etc/agent/prompt.md")
        );
        assert_eq!(config.tools, vec!["bash", "read_file"]);
        assert_eq!(config.socket_path, PathBuf::from("/run/tb.sock"));
        assert_eq!(config.max_iterations, 50);
        assert_eq!(config.max_output_chars, 10000);
    }

    #[test]
    fn defaults_for_optional_flags() {
        let a = args("--system-prompt /p.md --tools bash --socket /s.sock");
        let config = RuntimeConfig::from_args(&a).unwrap();
        assert_eq!(config.max_iterations, 100);
        assert_eq!(config.max_output_chars, 30000);
    }

    #[test]
    fn missing_required_flag_errors() {
        let a = args("--tools bash --socket /s.sock");
        assert!(RuntimeConfig::from_args(&a).is_err());

        let a = args("--system-prompt /p.md --socket /s.sock");
        assert!(RuntimeConfig::from_args(&a).is_err());

        let a = args("--system-prompt /p.md --tools bash");
        assert!(RuntimeConfig::from_args(&a).is_err());
    }

    #[test]
    fn unknown_flag_errors() {
        let a = args("--system-prompt /p.md --tools bash --socket /s.sock --bogus");
        assert!(RuntimeConfig::from_args(&a).is_err());
    }

    #[test]
    fn tools_split_by_comma() {
        let a = args("--system-prompt /p.md --tools bash,read_file,write_file,list_directory --socket /s.sock");
        let config = RuntimeConfig::from_args(&a).unwrap();
        assert_eq!(
            config.tools,
            vec!["bash", "read_file", "write_file", "list_directory"]
        );
    }

    #[test]
    fn double_comma_in_tools_filters_empty() {
        let a = vec![
            "--system-prompt".to_string(),
            "/p.md".to_string(),
            "--tools".to_string(),
            "bash,,read_file".to_string(),
            "--socket".to_string(),
            "/s.sock".to_string(),
        ];
        let config = RuntimeConfig::from_args(&a).unwrap();
        assert_eq!(config.tools, vec!["bash", "read_file"]);
    }

    #[test]
    fn invalid_max_iterations_errors() {
        let a = args("--system-prompt /p.md --tools bash --socket /s.sock --max-iterations abc");
        assert!(RuntimeConfig::from_args(&a).is_err());
    }

    #[test]
    fn negative_max_iterations_errors() {
        let a = args("--system-prompt /p.md --tools bash --socket /s.sock --max-iterations -1");
        assert!(RuntimeConfig::from_args(&a).is_err());
    }
}
