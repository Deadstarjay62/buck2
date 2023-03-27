/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

mod resolve_alias;
mod streaming;

use std::fmt::Write as _;
use std::fs::File;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use async_trait::async_trait;
use buck2_build_api::calculation::load_patterns;
use buck2_build_api::nodes::hacks::value_to_json;
use buck2_build_api::nodes::lookup::ConfiguredTargetNodeLookup;
use buck2_build_api::nodes::lookup::TargetNodeLookup;
use buck2_cli_proto::targets_request::TargetHashFileMode;
use buck2_cli_proto::targets_request::TargetHashGraphType;
use buck2_cli_proto::TargetsRequest;
use buck2_cli_proto::TargetsResponse;
use buck2_common::dice::cells::HasCellResolver;
use buck2_core::bzl::ImportPath;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::CellResolver;
use buck2_core::fs::paths::abs_path::AbsPath;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::package::PackageLabel;
use buck2_core::pattern::pattern_type::TargetPatternExtra;
use buck2_core::pattern::ParsedPattern;
use buck2_core::target::label::TargetLabel;
use buck2_node::attrs::inspect_options::AttrInspectOptions;
use buck2_node::nodes::attributes::DEPS;
use buck2_node::nodes::attributes::INPUTS;
use buck2_node::nodes::attributes::PACKAGE;
use buck2_node::nodes::attributes::TARGET_CALL_STACK;
use buck2_node::nodes::attributes::TARGET_HASH;
use buck2_node::nodes::attributes::TYPE;
use buck2_node::nodes::configured::ConfiguredTargetNode;
use buck2_node::nodes::unconfigured::TargetNode;
use buck2_server_ctx::ctx::ServerCommandContextTrait;
use buck2_server_ctx::partial_result_dispatcher::PartialResultDispatcher;
use buck2_server_ctx::pattern::parse_patterns_from_cli_args;
use buck2_server_ctx::pattern::target_platform_from_client_context;
use buck2_server_ctx::template::run_server_command;
use buck2_server_ctx::template::ServerCommandTemplate;
use dice::DiceTransaction;
use dupe::Dupe;
use dupe::OptionDupedExt;
use gazebo::prelude::*;
use itertools::Itertools;
use regex::RegexSet;

use crate::commands::targets::resolve_alias::targets_resolve_aliases;
use crate::commands::targets::streaming::targets_streaming;
use crate::json::quote_json_string;
use crate::target_hash::BuckTargetHash;
use crate::target_hash::TargetHashes;
use crate::target_hash::TargetHashesFileMode;

pub(crate) struct TargetInfo<'a> {
    node: &'a TargetNode,
    target_hash: Option<BuckTargetHash>,
}

#[allow(unused_variables)]
pub(crate) trait TargetFormatter: Send + Sync {
    fn begin(&self, buffer: &mut String) {}
    fn end(&self, stats: &Stats, buffer: &mut String) {}
    /// Called between each target/imports/package_error
    fn separator(&self, buffer: &mut String) {}
    fn target(&self, package: PackageLabel, target_info: TargetInfo<'_>, buffer: &mut String) {}
    fn imports(
        &self,
        source: &CellPath,
        imports: &[ImportPath],
        package: Option<PackageLabel>,
        buffer: &mut String,
    ) {
    }
    fn package_error(&self, package: PackageLabel, error: &anyhow::Error, buffer: &mut String) {}
}

struct JsonWriter {
    json_lines: bool,
}

impl JsonWriter {
    fn begin(&self, buffer: &mut String) {
        if !self.json_lines {
            buffer.push_str("[\n");
        }
    }

    fn end(&self, buffer: &mut String) {
        if !self.json_lines {
            buffer.push_str("\n]\n");
        }
    }

    fn separator(&self, buffer: &mut String) {
        if !self.json_lines {
            buffer.push_str(",\n");
        }
    }

    fn entry_start(&self, buffer: &mut String) {
        if self.json_lines {
            buffer.push('{');
        } else {
            buffer.push_str("  {\n");
        }
    }

    fn entry_end(&self, buffer: &mut String, first: bool) {
        if self.json_lines {
            buffer.push_str("}\n");
        } else {
            if !first {
                buffer.push('\n');
            }
            buffer.push_str("  }");
        }
    }

    fn entry_item(&self, buffer: &mut String, first: &mut bool, key: &str, value: &str) {
        if *first {
            *first = false;
        } else if self.json_lines {
            buffer.push(',');
        } else {
            buffer.push_str(",\n");
        }
        if self.json_lines {
            write!(buffer, "\"{}\":{}", key, value).unwrap();
        } else {
            write!(buffer, "    \"{}\": {}", key, value).unwrap();
        }
    }
}

struct JsonFormat {
    attributes: Option<RegexSet>,
    attr_inspect_opts: AttrInspectOptions,
    target_call_stacks: bool,
    writer: JsonWriter,
}

impl TargetFormatter for JsonFormat {
    fn begin(&self, buffer: &mut String) {
        self.writer.begin(buffer)
    }

    fn end(&self, _stats: &Stats, buffer: &mut String) {
        self.writer.end(buffer)
    }

    fn separator(&self, buffer: &mut String) {
        self.writer.separator(buffer)
    }

    fn target(&self, package: PackageLabel, target_info: TargetInfo<'_>, buffer: &mut String) {
        self.writer.entry_start(buffer);
        let mut first = true;

        fn print_attr(
            this: &JsonFormat,
            buffer: &mut String,
            first: &mut bool,
            k: &str,
            v: impl FnOnce() -> String,
        ) {
            if let Some(filter) = &this.attributes {
                if !filter.is_match(k) {
                    return;
                }
            }
            this.writer.entry_item(buffer, first, k, &v());
        }

        print_attr(self, buffer, &mut first, TYPE, || {
            quote_json_string(&target_info.node.rule_type().to_string())
        });
        print_attr(self, buffer, &mut first, DEPS, || {
            format!(
                "[{}]",
                target_info
                    .node
                    .deps()
                    .map(|d| quote_json_string(&d.to_string()))
                    .join(", ")
            )
        });

        print_attr(self, buffer, &mut first, INPUTS, || {
            format!(
                "[{}]",
                target_info
                    .node
                    .inputs()
                    .map(|x| quote_json_string(&x.to_string()))
                    .join(", ")
            )
        });

        if let Some(BuckTargetHash(hash)) = target_info.target_hash {
            print_attr(self, buffer, &mut first, TARGET_HASH, || {
                format!("\"{hash:032x}\"")
            });
        }
        print_attr(self, buffer, &mut first, PACKAGE, || {
            format!("\"{}\"", package)
        });

        for a in target_info.node.attrs(self.attr_inspect_opts) {
            print_attr(self, buffer, &mut first, a.name, || {
                value_to_json(a.value, target_info.node.label().pkg())
                    .unwrap()
                    .to_string()
            });
        }

        if self.target_call_stacks {
            match target_info.node.call_stack() {
                Some(call_stack) => {
                    print_attr(self, buffer, &mut first, TARGET_CALL_STACK, || {
                        quote_json_string(&call_stack)
                    });
                }
                None => {
                    // Should not happen.
                }
            }
        }
        self.writer.entry_end(buffer, first);
    }

    fn imports(
        &self,
        source: &CellPath,
        imports: &[ImportPath],
        package: Option<PackageLabel>,
        buffer: &mut String,
    ) {
        self.writer.entry_start(buffer);
        let mut first = true;
        if let Some(package) = package {
            self.writer.entry_item(
                buffer,
                &mut first,
                PACKAGE,
                &quote_json_string(&package.to_string()),
            );
        }
        self.writer.entry_item(
            buffer,
            &mut first,
            "buck.file",
            &quote_json_string(&source.to_string()),
        );
        self.writer.entry_item(
            buffer,
            &mut first,
            "buck.imports",
            &format!(
                "[{}]",
                imports
                    .map(|d| quote_json_string(&d.path().to_string()))
                    .join(", ")
            ),
        );
        self.writer.entry_end(buffer, first);
    }

    fn package_error(&self, package: PackageLabel, error: &anyhow::Error, buffer: &mut String) {
        self.writer.entry_start(buffer);
        let mut first = true;
        self.writer.entry_item(
            buffer,
            &mut first,
            PACKAGE,
            &quote_json_string(&package.to_string()),
        );
        self.writer.entry_item(
            buffer,
            &mut first,
            "buck.error",
            &quote_json_string(&format!("{:?}", error)),
        );
        self.writer.entry_end(buffer, first);
    }
}

#[derive(Debug, Default)]
pub(crate) struct Stats {
    errors: u64,
    success: u64,
    targets: u64,
}

impl Stats {
    fn merge(&mut self, stats: &Stats) {
        self.errors += stats.errors;
        self.success += stats.success;
        self.targets += stats.targets;
    }
}

struct StatsFormat;

impl TargetFormatter for StatsFormat {
    fn end(&self, stats: &Stats, buffer: &mut String) {
        writeln!(buffer, "{:?}", stats).unwrap()
    }
}

struct TargetNameFormat {
    target_call_stacks: bool,
    target_hash_graph_type: TargetHashGraphType,
}
impl TargetFormatter for TargetNameFormat {
    fn target(&self, package: PackageLabel, target_info: TargetInfo<'_>, buffer: &mut String) {
        if self.target_hash_graph_type != TargetHashGraphType::None {
            match target_info.target_hash {
                Some(BuckTargetHash(hash)) => writeln!(
                    buffer,
                    "{package}:{name} {hash:032x}",
                    name = target_info.node.label().name(),
                )
                .unwrap(),
                None => {} // print nothing if there is no hash and show_target_hash is specified.
            };
        } else {
            writeln!(buffer, "{}:{}", package, target_info.node.label().name()).unwrap();
        }
        if self.target_call_stacks {
            match target_info.node.call_stack() {
                Some(call_stack) => {
                    for line in call_stack.lines() {
                        writeln!(buffer, "  {}", line).unwrap();
                    }
                }
                None => {
                    // Should not happen.
                }
            }
        }
    }
}

struct TargetHashOptions {
    file_mode: TargetHashesFileMode,
    fast_hash: bool,
    graph_type: TargetHashGraphType,
    recursive: bool,
}

impl TargetHashOptions {
    fn new(
        request: &TargetsRequest,
        cell_resolver: &CellResolver,
        fs: &ProjectRoot,
    ) -> anyhow::Result<Self> {
        let file_mode = TargetHashFileMode::from_i32(request.target_hash_file_mode)
            .expect("buck cli should send valid target hash file mode");
        let file_mode = match file_mode {
            TargetHashFileMode::PathsOnly => {
                let modified_paths = request
                    .target_hash_modified_paths
                    .iter()
                    .map(|path| {
                        let path = AbsPath::new(Path::new(&path))?;
                        cell_resolver.get_cell_path_from_abs_path(path, fs)
                    })
                    .collect::<anyhow::Result<_>>()?;
                TargetHashesFileMode::PathsOnly(modified_paths)
            }
            TargetHashFileMode::PathsAndContents => TargetHashesFileMode::PathsAndContents,
            TargetHashFileMode::NoFiles => TargetHashesFileMode::None,
        };

        Ok(Self {
            file_mode,
            fast_hash: request.target_hash_use_fast_hash,
            graph_type: TargetHashGraphType::from_i32(request.target_hash_graph_type)
                .expect("buck cli should send valid target hash graph type"),
            recursive: request.target_hash_recursive,
        })
    }
}

pub(crate) enum Outputter {
    Stdout,
    File(BufWriter<File>),
}

impl Outputter {
    fn new(request: &TargetsRequest) -> anyhow::Result<Self> {
        match &request.output {
            None => Ok(Self::Stdout),
            Some(file) => Ok(Self::File(BufWriter::new(
                File::create(file).with_context(|| {
                    format!("Failed to open file `{}` for `targets` output ", file)
                })?,
            ))),
        }
    }

    fn write1(&mut self, stdout: &mut impl Write, x: &str) -> anyhow::Result<()> {
        match self {
            Self::Stdout => stdout.write_all(x.as_bytes())?,
            Self::File(f) => f.write_all(x.as_bytes())?,
        }
        Ok(())
    }

    fn write2(&mut self, stdout: &mut impl Write, x: &str, y: &str) -> anyhow::Result<()> {
        match self {
            Self::Stdout => {
                stdout.write_all(x.as_bytes())?;
                stdout.write_all(y.as_bytes())?;
            }
            Self::File(f) => {
                f.write_all(x.as_bytes())?;
                f.write_all(y.as_bytes())?;
            }
        }
        Ok(())
    }

    /// If this outputter should write anything to a file, do so, and return whatever buffer is left over.
    fn write_to_file(&mut self, buffer: String) -> anyhow::Result<String> {
        match self {
            Self::Stdout => Ok(buffer),
            Self::File(f) => {
                f.write_all(buffer.as_bytes())?;
                Ok(String::new())
            }
        }
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        match self {
            Self::Stdout => Ok(()),
            Self::File(f) => Ok(f.flush()?),
        }
    }
}

pub async fn targets_command(
    server_ctx: Box<dyn ServerCommandContextTrait>,
    partial_result_dispatcher: PartialResultDispatcher<buck2_cli_proto::StdoutBytes>,
    req: TargetsRequest,
) -> anyhow::Result<TargetsResponse> {
    run_server_command(
        TargetsServerCommand { req },
        server_ctx,
        partial_result_dispatcher,
    )
    .await
}

struct TargetsServerCommand {
    req: TargetsRequest,
}

#[async_trait]
impl ServerCommandTemplate for TargetsServerCommand {
    type StartEvent = buck2_data::TargetsCommandStart;
    type EndEvent = buck2_data::TargetsCommandEnd;
    type Response = TargetsResponse;
    type PartialResult = buck2_cli_proto::StdoutBytes;

    async fn command<'v>(
        &self,
        server_ctx: &'v dyn ServerCommandContextTrait,
        mut partial_result_dispatcher: PartialResultDispatcher<Self::PartialResult>,
        dice: DiceTransaction,
    ) -> anyhow::Result<Self::Response> {
        targets(
            server_ctx,
            &mut partial_result_dispatcher.as_writer(),
            dice,
            &self.req,
        )
        .await
    }

    fn is_success(&self, _response: &Self::Response) -> bool {
        // No response if we failed.
        true
    }
}

fn crate_formatter(request: &TargetsRequest) -> anyhow::Result<Arc<dyn TargetFormatter>> {
    let is_json = request.json || request.json_lines || !request.output_attributes.is_empty();
    if is_json {
        Ok(Arc::new(JsonFormat {
            attributes: if request.output_attributes.is_empty() {
                None
            } else {
                Some(RegexSet::new(&request.output_attributes)?)
            },
            attr_inspect_opts: if request.include_default_attributes {
                AttrInspectOptions::All
            } else {
                AttrInspectOptions::DefinedOnly
            },
            target_call_stacks: request.target_call_stacks,
            writer: JsonWriter {
                json_lines: request.json_lines,
            },
        }))
    } else if request.stats {
        Ok(Arc::new(StatsFormat))
    } else {
        Ok(Arc::new(TargetNameFormat {
            target_call_stacks: request.target_call_stacks,
            target_hash_graph_type: TargetHashGraphType::from_i32(request.target_hash_graph_type)
                .expect("buck cli should send valid target hash graph type"),
        }))
    }
}

async fn targets(
    server_ctx: &dyn ServerCommandContextTrait,
    stdout: &mut impl Write,
    dice: DiceTransaction,
    request: &TargetsRequest,
) -> anyhow::Result<TargetsResponse> {
    // TODO(nmj): Rather than returning fully formatted data in the TargetsResponse, we should
    //            instead return structured data, and return *that* to the CLI. The CLI should
    //            then handle printing. The current approach is just a temporary hack to fix some
    //            issues with printing to stdout.

    let cwd = server_ctx.working_dir();
    let cell_resolver = dice.get_cell_resolver().await?;
    let parsed_target_patterns =
        parse_patterns_from_cli_args::<TargetPatternExtra>(&dice, &request.target_patterns, cwd)
            .await?;

    let mut outputter = Outputter::new(request)?;

    let response = if request.unstable_resolve_aliases {
        targets_resolve_aliases(dice, request, parsed_target_patterns).await?
    } else if request.streaming {
        let formatter = crate_formatter(request)?;
        let hashing = match TargetHashGraphType::from_i32(request.target_hash_graph_type)
            .expect("buck cli should send valid target hash graph type")
        {
            TargetHashGraphType::None => None,
            _ => Some(request.target_hash_use_fast_hash),
        };

        let res = targets_streaming(
            server_ctx,
            stdout,
            dice,
            formatter,
            &mut outputter,
            parsed_target_patterns,
            request.keep_going,
            request.cached,
            request.imports,
            hashing,
        )
        .await;
        // Make sure we always flush the outputter, even on failure, as we may have partially written to it
        outputter.flush()?;
        res?
    } else {
        let formatter = crate_formatter(request)?;
        let target_platform =
            target_platform_from_client_context(request.context.as_ref(), &cell_resolver, cwd)
                .await?;
        let fs = server_ctx.project_root();
        targets_batch(
            server_ctx,
            dice,
            &*formatter,
            parsed_target_patterns,
            target_platform,
            TargetHashOptions::new(request, &cell_resolver, fs)?,
            request.keep_going,
        )
        .await?
    };
    let response = TargetsResponse {
        error_count: response.error_count,
        serialized_targets_output: outputter.write_to_file(response.serialized_targets_output)?,
    };
    outputter.flush()?;
    Ok(response)
}

fn mk_error(errors: u64) -> anyhow::Error {
    // Simpler error so that we don't print long errors twice (when exiting buck2)
    let package_str = if errors == 1 { "package" } else { "packages" };
    anyhow::anyhow!("Failed to parse {} {}", errors, package_str)
}

async fn targets_batch(
    server_ctx: &dyn ServerCommandContextTrait,
    dice: DiceTransaction,
    formatter: &dyn TargetFormatter,
    parsed_patterns: Vec<ParsedPattern<TargetPatternExtra>>,
    target_platform: Option<TargetLabel>,
    hash_options: TargetHashOptions,
    keep_going: bool,
) -> anyhow::Result<TargetsResponse> {
    let results = load_patterns(&dice, parsed_patterns).await?;

    let target_hashes = match hash_options.graph_type {
        TargetHashGraphType::Configured => Some(
            TargetHashes::compute::<ConfiguredTargetNode, _>(
                dice.dupe(),
                ConfiguredTargetNodeLookup(&dice),
                results.iter_loaded_targets_by_package().collect(),
                target_platform,
                hash_options.file_mode,
                hash_options.fast_hash,
                hash_options.recursive,
            )
            .await?,
        ),
        TargetHashGraphType::Unconfigured => Some(
            TargetHashes::compute::<TargetNode, _>(
                dice.dupe(),
                TargetNodeLookup(&dice),
                results.iter_loaded_targets_by_package().collect(),
                target_platform,
                hash_options.file_mode,
                hash_options.fast_hash,
                hash_options.recursive,
            )
            .await?,
        ),
        _ => None,
    };

    let mut buffer = String::new();
    formatter.begin(&mut buffer);
    let mut stats = Stats::default();
    let mut needs_separator = false;
    for (package, result) in results.iter() {
        match result {
            Ok(res) => {
                stats.success += 1;
                for (_, node) in res.iter() {
                    stats.targets += 1;
                    let target_hash = target_hashes
                        .as_ref()
                        .and_then(|hashes| hashes.get(node.label()))
                        .duped()
                        .transpose()?;
                    if needs_separator {
                        formatter.separator(&mut buffer);
                    }
                    needs_separator = true;
                    formatter.target(
                        package.dupe(),
                        TargetInfo { node, target_hash },
                        &mut buffer,
                    )
                }
            }
            Err(e) => {
                stats.errors += 1;
                if keep_going {
                    if needs_separator {
                        formatter.separator(&mut buffer);
                    }
                    needs_separator = true;
                    formatter.package_error(package.dupe(), e.inner(), &mut buffer);
                } else {
                    writeln!(
                        server_ctx.stderr()?,
                        "Error parsing {}\n{:?}",
                        package,
                        e.inner()
                    )?;
                }
            }
        }
    }
    formatter.end(&stats, &mut buffer);
    if !keep_going && stats.errors != 0 {
        Err(mk_error(stats.errors))
    } else {
        Ok(TargetsResponse {
            error_count: stats.errors,
            serialized_targets_output: buffer,
        })
    }
}