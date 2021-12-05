use crate::workflows::definitions::{WorkflowDefinition, WorkflowStepDefinition, WorkflowStepType};
use pest::iterators::{Pair, Pairs};
use pest::Parser;
use std::collections::HashMap;
use thiserror::Error;

/// Configuration for a Mmids system.  Defines the settings and any workflows that should be active.
pub struct MmidsConfig {
    pub settings: HashMap<String, Option<String>>,
    pub workflows: HashMap<String, WorkflowDefinition>,
}

/// Errors that can occur when parsing a configuration entry
#[derive(Error, Debug)]
pub enum ConfigParseError {
    #[error("The config provided could not be parsed")]
    InvalidConfig(#[from] pest::error::Error<Rule>),

    #[error("Found unexpected rule '{rule:?}' in the {section} section")]
    UnexpectedRule { rule: Rule, section: String },

    #[error("Duplicate workflow name: '{name}'")]
    DuplicateWorkflowName { name: String },

    #[error("Invalid node name '{name}' on line {line}")]
    InvalidNodeName { name: String, line: usize },

    #[error("Arguments are not allowed on a settings node, but some were found on line {line}")]
    ArgumentsSpecifiedOnSettingNode { line: usize },

    #[error("More than 1 argument was provided for the setting on line {line}")]
    TooManySettingArguments { line: usize },

    #[error("The argument provided for the setting on line {line} is invalid. Equal signs are not allowed")]
    InvalidSettingArgumentFormat { line: usize },

    #[error("Workflows should only have a single argument (it's name) but the workflow on line {line} had multiple")]
    TooManyWorkflowArguments { line: usize },

    #[error("The workflow on line {line} did not have a name specified")]
    NoNameOnWorkflow { line: usize },

    #[error("Invalid workflow name of {name} on line {line}")]
    InvalidWorkflowName { line: usize, name: String },
}

#[derive(Parser)]
#[grammar = "config.pest"]
struct RawConfigParser;

struct ChildNode {
    name: String,
    arguments: HashMap<String, Option<String>>,
}

/// Parses configuration from a text block.
pub fn parse(content: &str) -> Result<MmidsConfig, ConfigParseError> {
    let mut config = MmidsConfig {
        settings: HashMap::new(),
        workflows: HashMap::new(),
    };

    let pairs = RawConfigParser::parse(Rule::content, content)?;
    for pair in pairs {
        let rule = pair.as_rule();
        match &rule {
            Rule::node_block => handle_node_block(&mut config, pair)?,
            Rule::EOI => (),
            x => {
                return Err(ConfigParseError::UnexpectedRule {
                    rule: x.clone(),
                    section: "root".to_string(),
                })
            }
        }
    }

    Ok(config)
}

fn handle_node_block(config: &mut MmidsConfig, pair: Pair<Rule>) -> Result<(), ConfigParseError> {
    let mut rules = pair.into_inner();
    let name_node = rules.next().unwrap(); // grammar requires a node name
    let name = name_node.as_str().trim();

    match name.to_lowercase().as_str() {
        "settings" => read_settings(config, rules)?,
        "workflow" => read_workflow(config, rules, name_node.as_span().start_pos().line_col().0)?,
        _ => {
            return Err(ConfigParseError::InvalidNodeName {
                name: name.to_string(),
                line: name_node.as_span().start_pos().line_col().0,
            })
        }
    }

    Ok(())
}

fn read_settings(config: &mut MmidsConfig, pairs: Pairs<Rule>) -> Result<(), ConfigParseError> {
    for pair in pairs {
        match pair.as_rule() {
            Rule::child_node => {
                let child_node = read_child_node(pair.clone())?;
                if child_node.arguments.len() > 1 {
                    return Err(ConfigParseError::TooManySettingArguments {
                        line: pair.as_span().start_pos().line_col().0,
                    });
                }

                if let Some(key) = child_node.arguments.keys().nth(0) {
                    if let Some(Some(_value)) = child_node.arguments.get(key) {
                        return Err(ConfigParseError::InvalidSettingArgumentFormat {
                            line: pair.as_span().start_pos().line_col().0,
                        });
                    }

                    config.settings.insert(child_node.name, Some(key.clone()));
                } else {
                    config.settings.insert(child_node.name, None);
                }
            }

            Rule::argument => {
                return Err(ConfigParseError::ArgumentsSpecifiedOnSettingNode {
                    line: pair.as_span().start_pos().line_col().0,
                })
            }

            rule => {
                return Err(ConfigParseError::UnexpectedRule {
                    rule,
                    section: "settings".to_string(),
                })
            }
        }
    }

    Ok(())
}

fn read_workflow(
    config: &mut MmidsConfig,
    pairs: Pairs<Rule>,
    starting_line: usize,
) -> Result<(), ConfigParseError> {
    let mut steps = Vec::new();
    let mut workflow_name = None;
    for pair in pairs {
        match pair.as_rule() {
            Rule::child_node => {
                let child_node = read_child_node(pair)?;
                steps.push(WorkflowStepDefinition {
                    step_type: WorkflowStepType(child_node.name),
                    parameters: child_node.arguments,
                });
            }

            Rule::argument => {
                if workflow_name.is_some() {
                    return Err(ConfigParseError::TooManyWorkflowArguments {
                        line: pair.as_span().start_pos().line_col().0,
                    });
                }

                let (key, value) = read_argument(pair.clone())?;
                if value.is_some() {
                    return Err(ConfigParseError::InvalidWorkflowName {
                        name: pair.as_str().to_string(),
                        line: pair.as_span().start_pos().line_col().0,
                    });
                }

                workflow_name = Some(key);
            }

            rule => {
                return Err(ConfigParseError::UnexpectedRule {
                    rule,
                    section: "workflow".to_string(),
                })
            }
        }
    }

    if let Some(name) = workflow_name {
        if config.workflows.contains_key(&name) {
            return Err(ConfigParseError::DuplicateWorkflowName { name });
        }

        config
            .workflows
            .insert(name.to_string(), WorkflowDefinition { name, steps });
    } else {
        return Err(ConfigParseError::NoNameOnWorkflow {
            line: starting_line,
        });
    }

    Ok(())
}

fn read_argument(pair: Pair<Rule>) -> Result<(String, Option<String>), ConfigParseError> {
    let result;
    // Each argument should have a single child rule based on grammar
    let argument = pair.into_inner().nth(0).unwrap();
    match argument.as_rule() {
        Rule::argument_flag => {
            result = (argument.as_str().to_string(), None);
        }

        Rule::quoted_string_value => {
            result = (argument.as_str().to_string(), None);
        }

        Rule::key_value_pair => {
            let mut key = "".to_string();
            let mut value = "".to_string();
            for inner in argument.into_inner() {
                match inner.as_rule() {
                    Rule::key => key = inner.as_str().to_string(),
                    Rule::value => {
                        // If this is a quotes string value, we need to unquote it, otherwise
                        // use the value as-is
                        value = inner
                            .clone()
                            .into_inner()
                            .filter(|p| p.as_rule() == Rule::quoted_string_value)
                            .map(|p| p.as_str().to_string())
                            .nth(0)
                            .unwrap_or(inner.as_str().to_string());
                    }

                    rule => {
                        return Err(ConfigParseError::UnexpectedRule {
                            rule,
                            section: "argument".to_string(),
                        })
                    }
                }
            }

            result = (key, Some(value));
        }

        _ => {
            return Err(ConfigParseError::UnexpectedRule {
                rule: argument.as_rule(),
                section: "child_node argument".to_string(),
            })
        }
    }

    Ok(result)
}

fn read_child_node(child_node: Pair<Rule>) -> Result<ChildNode, ConfigParseError> {
    let mut pairs = child_node.into_inner();
    let name_node = pairs.next().unwrap(); // Grammar requires a node name first
    let mut parsed_node = ChildNode {
        name: name_node.as_str().to_string(),
        arguments: HashMap::new(),
    };

    for pair in pairs {
        match pair.as_rule() {
            Rule::argument => {
                let (key, value) = read_argument(pair)?;
                parsed_node.arguments.insert(key, value);
            }

            rule => {
                return Err(ConfigParseError::UnexpectedRule {
                    rule,
                    section: "child_node".to_string(),
                })
            }
        }
    }

    Ok(parsed_node)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_parse_settings() {
        let content = "
settings {
    first a
    second \"C:\\program files\\ffmpeg\\bin\\ffmpeg.exe\"
    flag

}
";

        let config = parse(content).unwrap();
        assert_eq!(config.settings.len(), 3, "Unexpected number of settings");
        assert_eq!(
            config.settings.get("first"),
            Some(&Some("a".to_string())),
            "Unexpected first value"
        );
        assert_eq!(
            config.settings.get("second"),
            Some(&Some(
                "C:\\program files\\ffmpeg\\bin\\ffmpeg.exe".to_string()
            )),
            "Unexpected second value"
        );
        assert_eq!(
            config.settings.get("flag"),
            Some(&None),
            "Unexpected flag value"
        );
    }

    #[test]
    fn can_read_single_workflow() {
        let content = "
workflow name {
    rtmp_receive port=1935 app=receive stream_key=*
    hls path=c:\\temp\\test.m3u8 segment_size=\"3\" size=640x480 flag
}
";
        let config = parse(content).unwrap();
        assert_eq!(config.workflows.len(), 1, "Unexpected number of workflows");
        assert!(
            config.workflows.contains_key("name"),
            "workflow 'name' did not exist"
        );

        let workflow = config.workflows.get("name").unwrap();
        assert_eq!(
            workflow.name,
            "name".to_string(),
            "Unexpected workflow name"
        );
        assert_eq!(
            workflow.steps.len(),
            2,
            "Unexpected number of workflow steps"
        );

        let step1 = workflow.steps.get(0).unwrap();
        assert_eq!(
            step1.step_type.0,
            "rtmp_receive".to_string(),
            "Unexpected type of step 1"
        );
        assert_eq!(step1.parameters.len(), 3, "Unexpected number of parameters");
        assert_eq!(
            step1.parameters.get("port"),
            Some(&Some("1935".to_string())),
            "Unexpected step 1 port value"
        );
        assert_eq!(
            step1.parameters.get("app"),
            Some(&Some("receive".to_string())),
            "Unexpected step 1 app value"
        );
        assert_eq!(
            step1.parameters.get("stream_key"),
            Some(&Some("*".to_string())),
            "Unexpected step 1 stream_key value"
        );

        let step2 = workflow.steps.get(1).unwrap();
        assert_eq!(
            step2.step_type.0,
            "hls".to_string(),
            "Unexpected type of step 1"
        );
        assert_eq!(step2.parameters.len(), 4, "Unexpected number of parameters");
        assert_eq!(
            step2.parameters.get("path"),
            Some(&Some("c:\\temp\\test.m3u8".to_string())),
            "Unexpected step 2 path value"
        );
        assert_eq!(
            step2.parameters.get("segment_size"),
            Some(&Some("3".to_string())),
            "Unexpected step 2 segment_size value"
        );
        assert_eq!(
            step2.parameters.get("size"),
            Some(&Some("640x480".to_string())),
            "Unexpected step 2 size value"
        );
        assert_eq!(
            step2.parameters.get("flag"),
            Some(&None),
            "Unexpected step 2 flag value"
        );
    }

    #[test]
    fn can_read_multiple_workflows() {
        let content = "
workflow name {
    rtmp_receive port=1935 app=receive stream_key=*
    hls path=c:\\temp\\test.m3u8 segment_size=\"3\" size=640x480 flag
}

workflow name2 {
    another a
}
";
        let config = parse(content).unwrap();

        assert_eq!(config.workflows.len(), 2, "Unexpected number of workflows");
        assert!(
            config.workflows.contains_key("name"),
            "Could not find a workflow named 'name'"
        );
        assert!(
            config.workflows.contains_key("name2"),
            "Could not find a workflow named 'name2'"
        );
    }

    #[test]
    fn duplicate_workflow_name_returns_error() {
        let content = "
workflow name {
    rtmp_receive port=1935 app=receive stream_key=*
    hls path=c:\\temp\\test.m3u8 segment_size=\"3\" size=640x480 flag
}

workflow name {
    another a
}
";
        match parse(content) {
            Err(ConfigParseError::DuplicateWorkflowName { name }) => {
                if name != "name".to_string() {
                    panic!("Unexpected name in workflow: '{}'", name);
                }
            }
            Err(e) => panic!(
                "Expected duplicate workflow name error, instead got: {:?}",
                e
            ),
            Ok(_) => panic!("Received successful parse, but an error was expected"),
        }
    }

    #[test]
    fn full_config_can_be_parsed() {
        let content = "
# comment
settings {
    first a # another comment
    second \"C:\\program files\\ffmpeg\\bin\\ffmpeg.exe\"
    flag

}

workflow name { #workflow comment
    rtmp_receive port=1935 app=receive stream_key=* #step comment
    hls path=c:\\temp\\test.m3u8 segment_size=\"3\" size=640x480 flag
}

workflow name2 {
    another a
}
";
        parse(content).unwrap();
    }
}
