//! Stdio MCP adapter for configured business commands.

use std::{borrow::Cow, collections::BTreeMap, sync::Arc};

use rmcp::{
    ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, DiscoverResult,
        ErrorData, Implementation, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
        ServerCapabilities, ServerConfig, Tool,
    },
    service::{RequestContext, RoleServer},
};
use serde_json::{Map, Value, json};

#[derive(Debug)]
struct State {
    config: Option<crate::config::Config>,
    specs: Vec<crate::command::CommandSpec>,
    names: BTreeMap<String, usize>,
    tools: Vec<Tool>,
    load_error: Option<String>,
    logger: crate::log::Logger,
}

#[derive(Debug, Clone)]
pub(crate) struct Server {
    state: Arc<State>,
    serial: Arc<tokio::sync::Semaphore>,
}

impl Server {
    pub(crate) fn new(
        loaded: crate::Result<(crate::config::Config, Vec<crate::command::CommandSpec>)>,
        logger: crate::log::Logger,
    ) -> crate::Result<Self> {
        let (mut config, mut specs, mut load_error) = match loaded {
            Ok((config, specs)) => (Some(config), specs, None),
            Err(err) => (None, Vec::new(), Some(err.to_string())),
        };
        let mut names = BTreeMap::new();
        for (index, spec) in specs.iter().enumerate() {
            let name = spec.path.join("_");
            if let Some(previous) = names.insert(name.clone(), index) {
                return Err(crate::Error::config(format!(
                    "MCP tool name \"{name}\" collides between {} and {}",
                    specs[previous].file.display(),
                    spec.file.display()
                )));
            }
        }
        let tools = specs
            .iter()
            .map(|spec| tool_for(spec, spec.path.join("_")))
            .collect::<crate::Result<Vec<_>>>();
        let tools = match tools {
            Ok(tools) => tools,
            Err(err) => {
                load_error = Some(err.to_string());
                config = None;
                specs.clear();
                names.clear();
                Vec::new()
            }
        };
        Ok(Self {
            state: Arc::new(State {
                config,
                specs,
                names,
                tools,
                load_error,
                logger,
            }),
            serial: Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }

    fn validate_call(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<(usize, BTreeMap<String, String>, String), ErrorData> {
        let index = *self.state.names.get(request.name.as_ref()).ok_or_else(|| {
            ErrorData::invalid_params(format!("unknown MCP tool: {}", request.name), None)
        })?;
        let spec = &self.state.specs[index];
        let supplied = request.arguments.as_ref();
        if matches!(spec.input, crate::command::InputMode::File)
            && !supplied.is_some_and(|arguments| arguments.contains_key("mcp.input"))
        {
            return Err(ErrorData::invalid_params(
                "missing required argument: mcp.input",
                None,
            ));
        }
        let mut args = BTreeMap::new();
        let mut input = String::new();
        if let Some(supplied) = supplied {
            for (name, value) in supplied {
                if name == "mcp.input" {
                    input = value
                        .as_str()
                        .ok_or_else(|| {
                            ErrorData::invalid_params("mcp.input must be a string", None)
                        })?
                        .to_string();
                    continue;
                }
                let declaration = spec.args.get(name).ok_or_else(|| {
                    ErrorData::invalid_params(format!("unknown argument: {name}"), None)
                })?;
                let parsed = if matches!(declaration.kind, crate::command::ArgType::Integer) {
                    let number = value.as_i64().ok_or_else(|| {
                        ErrorData::invalid_params(
                            format!("argument {name} must be an integer"),
                            None,
                        )
                    })?;
                    if declaration.min.is_some_and(|min| number < min)
                        || declaration.max.is_some_and(|max| number > max)
                    {
                        return Err(ErrorData::invalid_params(
                            format!("argument {name} is outside its bounds"),
                            None,
                        ));
                    }
                    number.to_string()
                } else {
                    let text = value.as_str().ok_or_else(|| {
                        ErrorData::invalid_params(format!("argument {name} must be a string"), None)
                    })?;
                    if declaration
                        .values
                        .as_ref()
                        .is_some_and(|values| !values.iter().any(|value| value == text))
                    {
                        return Err(ErrorData::invalid_params(
                            format!("argument {name} is not an allowed value"),
                            None,
                        ));
                    }
                    text.to_string()
                };
                args.insert(name.clone(), parsed);
            }
        }
        for (name, declaration) in &spec.args {
            if declaration.required && !args.contains_key(name) {
                return Err(ErrorData::invalid_params(
                    format!("missing required argument: {name}"),
                    None,
                ));
            }
        }
        Ok((index, args, input))
    }
}

fn tool_for(spec: &crate::command::CommandSpec, name: String) -> crate::Result<Tool> {
    let mut properties = Map::new();
    let mut required = Vec::new();
    if matches!(spec.input, crate::command::InputMode::File) {
        required.push("mcp.input".to_string());
    }
    properties.insert(
        "mcp.input".to_string(),
        json!({
            "type": "string", "description": "Command input text"
        }),
    );
    for (name, declaration) in &spec.args {
        let mut schema = Map::new();
        schema.insert(
            "type".into(),
            json!(
                if matches!(declaration.kind, crate::command::ArgType::Integer) {
                    "integer"
                } else {
                    "string"
                }
            ),
        );
        schema.insert(
            "description".into(),
            json!(
                if matches!(declaration.kind, crate::command::ArgType::File) {
                    format!("Content of {}", declaration.description)
                } else {
                    declaration.description.clone()
                }
            ),
        );
        if let Some(values) = &declaration.values {
            schema.insert("enum".into(), json!(values));
        }
        if let Some(min) = declaration.min {
            schema.insert("minimum".into(), json!(min));
        }
        if let Some(max) = declaration.max {
            schema.insert("maximum".into(), json!(max));
        }
        properties.insert(name.clone(), Value::Object(schema));
        if declaration.required {
            required.push(name.clone());
        }
    }
    let input_schema = json!({
        "type": "object", "properties": properties, "required": required,
        "additionalProperties": false
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    let output_schema = spec
        .output
        .schema
        .as_ref()
        .map(|path| {
            crate::output::read_schema(path, &spec.file).and_then(|schema| {
                schema.as_object().cloned().ok_or_else(|| {
                    crate::Error::config(format!(
                        "{}: output schema must be an object",
                        path.display()
                    ))
                })
            })
        })
        .transpose()?;
    let mut tool = Tool::default();
    tool.name = Cow::Owned(name);
    tool.description = Some(Cow::Owned(spec.description.clone()));
    tool.input_schema = Arc::new(input_schema);
    tool.output_schema = output_schema.map(Arc::new);
    Ok(tool)
}

impl ServerHandler for Server {
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&[ProtocolVersion::V_2026_07_28])
    }

    fn get_info(&self) -> ServerConfig {
        let info = ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::V_2026_07_28)
            .with_server_info(Implementation::new("npu", env!("CARGO_PKG_VERSION")));
        if let Some(error) = &self.state.load_error {
            info.with_instructions(format!(
                "Configuration failed to load: {error}. Run npu doctor, then restart this server."
            ))
        } else {
            info.with_instructions(
                "Tools reflect configuration loaded at startup; restart to reload.",
            )
        }
    }

    fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<DiscoverResult, ErrorData>> {
        std::future::ready(Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            self.get_info(),
        )))
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> {
        std::future::ready(Ok(ListToolsResult {
            tools: self.state.tools.clone(),
            ..ListToolsResult::default()
        }))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.state
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let (index, args, input) = self.validate_call(&request)?;
        let _permit = self
            .serial
            .acquire()
            .await
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        let state = Arc::clone(&self.state);
        let result = tokio::task::spawn_blocking(move || {
            let spec = &state.specs[index];
            let config = state
                .config
                .as_ref()
                .ok_or_else(|| crate::Error::config("configuration unavailable"))?;
            crate::exec::execute_mcp_command(spec, config, &args, &input, state.logger).map(
                |output| {
                    (
                        output,
                        matches!(spec.output.format, crate::output::Format::Json),
                    )
                },
            )
        })
        .await
        .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        let response = match result {
            Ok((output, json_output)) => {
                let mut response =
                    CallToolResult::success(vec![ContentBlock::text(output.clone())]);
                if json_output {
                    response.structured_content = serde_json::from_str(&output).ok();
                }
                response
            }
            Err(err) => {
                let mut response = CallToolResult::error(vec![ContentBlock::text(err.to_string())]);
                response.structured_content = Some(err.envelope());
                response
            }
        };
        Ok(response.into())
    }
}

pub(crate) fn serve(server: Server) -> crate::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(crate::Error::Io)?;
    runtime.block_on(async move {
        let running = server
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|err| crate::Error::backend(format!("MCP transport failed: {err}")))?;
        running
            .waiting()
            .await
            .map_err(|err| crate::Error::backend(format!("MCP server failed: {err}")))?;
        Ok(())
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn spec(path: &[&str], file: &str) -> crate::command::CommandSpec {
        let mut spec = crate::command::parse(
            "---\nmodel = \"test\"\n\n[args.input]\nrequired = true\n---\n{{ args.input }} {{ input }}\n",
            path.iter().map(ToString::to_string).collect(),
            std::path::Path::new("."),
        )
        .expect("valid fixture");
        spec.file = file.into();
        spec
    }

    #[test]
    fn collision_names_both_source_files() {
        let err = Server::new(
            Ok((
                crate::config::Config::default(),
                vec![
                    spec(&["git", "review"], "first.md"),
                    spec(&["git_review"], "second.md"),
                ],
            )),
            crate::log::Logger::new(crate::log::Level::Error),
        )
        .expect_err("tool names collide");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("first.md"));
        assert!(err.to_string().contains("second.md"));
    }

    #[test]
    fn literal_mcp_input_does_not_shadow_configured_input_argument() {
        let server = Server::new(
            Ok((
                crate::config::Config::default(),
                vec![spec(&["translate"], "translate.md")],
            )),
            crate::log::Logger::new(crate::log::Level::Error),
        )
        .expect("valid server");
        let mut params = CallToolRequestParams::new("translate");
        params.arguments = Some(
            serde_json::from_value(json!({
                "input": "configured argument", "mcp.input": "text input"
            }))
            .expect("object"),
        );
        let (_, args, input) = server.validate_call(&params).expect("valid call");
        assert_eq!(
            args.get("input").map(String::as_str),
            Some("configured argument")
        );
        assert_eq!(input, "text input");
    }

    #[test]
    fn degraded_server_lists_no_tools_and_gives_repair_instruction() {
        let server = Server::new(
            Err(crate::Error::config("bad command file")),
            crate::log::Logger::new(crate::log::Level::Error),
        )
        .expect("degraded mode starts");
        assert!(server.state.tools.is_empty());
        assert!(
            server
                .get_info()
                .instructions
                .is_some_and(|text| text.contains("npu doctor"))
        );
    }

    #[test]
    fn unreadable_output_schema_degrades_the_server() {
        let mut command = spec(&["review"], "review.md");
        command.output.schema = Some("missing-schema.json".into());
        let server = Server::new(
            Ok((crate::config::Config::default(), vec![command])),
            crate::log::Logger::new(crate::log::Level::Error),
        )
        .expect("invalid schema degrades instead of stopping the server");
        assert!(server.state.tools.is_empty());
        assert!(
            server
                .get_info()
                .instructions
                .is_some_and(|text| text.contains("review.md"))
        );
    }
}
