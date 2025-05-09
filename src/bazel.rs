/*
 * Copyright 2019 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! This file provides an implementation of starlark-rust's `LspContext` aimed at
//! the use in a Bazel project.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::ops::Deref;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::anyhow;
use lsp_types::CompletionItemKind;
use lsp_types::Url;
use prost::Message;
use starlark::analysis::find_call_name::AstModuleFindCallName;
use starlark::analysis::AstModuleLint;
use starlark::collections::SmallMap;
use starlark::docs::DocItem;
use starlark::docs::DocModule;
use starlark::errors::EvalMessage;
use starlark::syntax::AstModule;
use starlark::syntax::Dialect;
use starlark_lsp::completion::StringCompletionResult;
use starlark_lsp::completion::StringCompletionType;
use starlark_lsp::error::eval_message_to_lsp_diagnostic;
use starlark_lsp::server::LspContext;
use starlark_lsp::server::LspEvalResult;
use starlark_lsp::server::LspUrl;
use starlark_lsp::server::StringLiteralResult;
use starlark_syntax::slice_vec_ext::VecExt;

use crate::builtin;
use crate::client::BazelClient;
use crate::file_type::FileType;
use crate::label::Label;
use crate::workspace::BazelWorkspace;

#[derive(Debug, thiserror::Error)]
enum ContextError {
    /// The provided Url was not absolute and it needs to be.
    #[error("Path for URL `{}` was not absolute", .0)]
    NotAbsolute(LspUrl),
    /// The scheme provided was not correct or supported.
    #[error("Url `{}` was expected to be of type `{}`", .1, .0)]
    WrongScheme(String, LspUrl),
}

/// Errors when [`LspContext::resolve_load()`] cannot resolve a given path.
#[derive(thiserror::Error, Debug)]
enum ResolveLoadError {
    /// Attempted to resolve a relative path, but no current_file_path was provided,
    /// so it is not known what to resolve the path against.
    #[error("Relative label `{}` provided, but current_file_path could not be determined", .0)]
    MissingCurrentFilePath(Label),
    /// The scheme provided was not correct or supported.
    #[error("Url `{}` was expected to be of type `{}`", .1, .0)]
    WrongScheme(String, LspUrl),
    /// Received a load for an absolute path from the root of the workspace, but the
    /// path to the workspace root was not provided.
    #[error("Label `{}` is absolute from the root of the workspace, but no workspace root was provided", .0)]
    MissingWorkspaceRoot(Label),
    /// The path contained a repository name that is not known to Bazel.
    #[error("Cannot resolve label `{}` because the repository `{}` is unknown", .0, .1)]
    UnknownRepository(Label, String),
    /// The path contained a target name that does not resolve to an existing file.
    #[error("Cannot resolve path `{}` because the file does not exist", .0)]
    TargetNotFound(String),
}

/// Errors when [`LspContext::render_as_load()`] cannot render a given path.
#[derive(thiserror::Error, Debug)]
enum RenderLoadError {
    /// Attempted to get the filename of a path that does not seem to contain a filename.
    #[error("Path `{}` provided, which does not seem to contain a filename", .0.display())]
    MissingTargetFilename(PathBuf),
    /// The scheme provided was not correct or supported.
    #[error("Urls `{}` and `{}` was expected to be of type `{}`", .1, .2, .0)]
    WrongScheme(String, LspUrl, LspUrl),
}

/// Starting point for resolving filesystem completions.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FilesystemCompletionRoot<'a> {
    /// A resolved path, e.g. from an opened document.
    Path(&'a Path),
    /// An unresolved path, e.g. from a string literal in a `load` statement.
    String(&'a str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FilesystemFileCompletionOptions {
    All,
    OnlyLoadable,
    None,
}

/// Options for resolving filesystem completions.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FilesystemCompletionOptions {
    /// Whether to include directories in the results.
    directories: bool,
    /// Whether to include files in the results.
    files: FilesystemFileCompletionOptions,
    /// Whether to include target names from BUILD files.
    targets: bool,
}

pub(crate) struct BazelContext<Client> {
    workspaces: RefCell<HashMap<PathBuf, Rc<BazelWorkspace>>>,
    query_output_base: Option<PathBuf>,
    pub(crate) client: Client,
}

fn is_workspace_file(uri: &LspUrl) -> bool {
    match uri {
        LspUrl::File(path) => path
            .file_name()
            .map(|name| name == "WORKSPACE" || name == "WORKSPACE.bazel")
            .unwrap_or(false),
        LspUrl::Starlark(_) => false,
        LspUrl::Other(_) => false,
    }
}

impl<Client: BazelClient> BazelContext<Client> {
    pub(crate) fn new(client: Client, query_output_base: Option<PathBuf>) -> anyhow::Result<Self> {
        Ok(Self {
            workspaces: RefCell::new(HashMap::new()),
            query_output_base,
            client,
        })
    }

    fn lint_module(&self, uri: &LspUrl, ast: &AstModule) -> Vec<EvalMessage> {
        let globals = self.get_bazel_globals_names(uri);

        let is_workspace_file = is_workspace_file(uri);

        ast.lint(Some(globals).as_ref())
            .into_iter()
            .filter(|lint| !(is_workspace_file && lint.short_name == "misplaced-load"))
            .map(EvalMessage::from)
            .collect()
    }

    /// Gets the possibly-cached workspace for a directory, or creates a new one if it doesn't exist.
    /// If the workspace is not given, it is inferred based on the current file.
    /// Returns None if a workspace cannot be found.
    fn workspace<P: AsRef<Path>>(
        &self,
        workspace_dir: Option<P>,
        current_file: &LspUrl,
    ) -> anyhow::Result<Option<Rc<BazelWorkspace>>> {
        let mut workspaces = self.workspaces.borrow_mut();

        let workspace_dir = match workspace_dir.as_ref() {
            Some(workspace_dir) => Some(Cow::Borrowed(workspace_dir.as_ref())),
            None => self.infer_workspace_dir(current_file)?.map(Cow::Owned),
        };

        if let Some(workspace_dir) = workspace_dir {
            if let Some(workspace) = workspaces.get(workspace_dir.as_ref()) {
                Ok(Some(workspace.clone()))
            } else {
                let info = self.client.info(workspace_dir.as_ref())?;

                let workspace =
                    BazelWorkspace::from_bazel_info(info, self.query_output_base.as_deref())?;

                workspaces.insert(workspace_dir.as_ref().to_owned(), Rc::new(workspace));

                Ok(workspaces.get(workspace_dir.as_ref()).map(|ws| ws.clone()))
            }
        } else {
            Ok(None)
        }
    }

    fn infer_workspace_dir(&self, current_file: &LspUrl) -> io::Result<Option<PathBuf>> {
        if let LspUrl::File(path) = current_file {
            for dir in path.ancestors().skip(1) {
                let file = dir.join("DO_NOT_BUILD_HERE");
                if file.exists() {
                    return Ok(Some(PathBuf::from(fs::read_to_string(file)?)));
                }
            }

            Ok(None)
        } else {
            Ok(None)
        }
    }

    // TODO: Consider caching this
    fn repo_mapping_for_file(
        &self,
        workspace: &BazelWorkspace,
        current_file: &LspUrl,
    ) -> anyhow::Result<HashMap<String, String>> {
        let current_repository = workspace
            .get_repository_for_lspurl(current_file)
            .unwrap_or(Cow::Borrowed(""));

        self.client
            .dump_repo_mapping(workspace, &current_repository)
    }

    /// Finds the directory that is the root of a package, given a label
    fn resolve_folder<'a>(
        &self,
        label: &Label,
        current_file: &LspUrl,
        workspace: Option<&BazelWorkspace>,
    ) -> anyhow::Result<PathBuf> {
        // Find the root we're resolving from. There's quite a few cases to consider here:
        // - `repository` is empty, and we're resolving from the workspace root.
        // - `repository` is empty, and we're resolving from a known remote repository.
        // - `repository` is not empty, and refers to the current repository (the workspace).
        // - `repository` is not empty, and refers to a known remote repository.
        //
        // Also with all of these cases, we need to consider if we have build system
        // information or not. If not, we can't resolve any remote repositories, and we can't
        // know whether a repository name refers to the workspace or not.
        let resolve_root = match &label.repo {
            // Repository is empty. If we know what file we're resolving from, use the build
            // system information to check if we're in a known remote repository, and what the
            // root is. Fall back to the `workspace_root` otherwise.
            None => {
                if let Some(repository_name) =
                    workspace.and_then(|ws| ws.get_repository_for_lspurl(current_file))
                {
                    workspace
                        .map(|ws| ws.get_repository_path(&repository_name))
                        .map(Cow::Owned)
                } else {
                    workspace.map(|ws| Cow::Borrowed(&ws.root))
                }
            }
            // We have a repository name and build system information. Check if the repository
            // name refers to the workspace, and if so, use the workspace root. If not, check
            // if it refers to a known remote repository, and if so, use that root.
            // Otherwise, fail with an error.
            Some(repository) => {
                // If we are navigating to another repository, we need to apply the repo mapping.
                // The repo mapping depends on the current repository, so resolve that first.
                let repo_mapping = workspace
                    .and_then(|ws| self.repo_mapping_for_file(ws, current_file).ok())
                    .unwrap_or_default();

                let remote_repository_name = repo_mapping
                    .get(&repository.name)
                    .unwrap_or(&repository.name);

                if matches!(workspace, Some(ws) if ws.workspace_name.as_ref() == Some(&repository.name))
                {
                    workspace.map(|ws| Cow::Borrowed(&ws.root))
                } else if let Some(remote_repository_root) = workspace
                    .map(|ws| ws.get_repository_path(remote_repository_name))
                    .map(Cow::Owned)
                {
                    Some(remote_repository_root)
                } else {
                    return Err(ResolveLoadError::UnknownRepository(
                        label.clone(),
                        repository.name.clone(),
                    )
                    .into());
                }
            }
        };

        if let Some(package) = &label.package {
            // Resolve from the root of the repository.
            match resolve_root {
                Some(resolve_root) => Ok(resolve_root.join(package)),
                None => Err(ResolveLoadError::MissingWorkspaceRoot(label.clone()).into()),
            }
        } else {
            // If we don't have a package, this is relative to the current file,
            // so resolve relative paths from the current file.
            match current_file {
                LspUrl::File(current_file_path) => {
                    let current_file_dir = current_file_path.parent();
                    match current_file_dir {
                        Some(current_file_dir) => Ok(current_file_dir.to_owned()),
                        None => Err(ResolveLoadError::MissingCurrentFilePath(label.clone()).into()),
                    }
                }
                _ => Err(
                    ResolveLoadError::WrongScheme("file://".to_owned(), current_file.clone())
                        .into(),
                ),
            }
        }
    }

    fn get_filesystem_entries(
        &self,
        from: FilesystemCompletionRoot,
        current_file: &LspUrl,
        workspace: Option<&BazelWorkspace>,
        options: &FilesystemCompletionOptions,
        results: &mut Vec<StringCompletionResult>,
    ) -> anyhow::Result<()> {
        // Find the actual folder on disk we're looking at.
        let (from_path, render_base) = match from {
            FilesystemCompletionRoot::Path(path) => (path.to_owned(), ""),
            FilesystemCompletionRoot::String(str) => {
                let label = Label::parse(str)?;
                (self.resolve_folder(&label, current_file, workspace)?, str)
            }
        };

        for entry in fs::read_dir(from_path)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = FileType::from_path(&path);

            // NOTE: Safe to `unwrap()` here, because we know that `path` is a file system path. And
            // since it's an entry in a directory, it must have a file name.
            let file_name = path.file_name().unwrap().to_string_lossy();
            if path.is_dir() && options.directories {
                results.push(StringCompletionResult {
                    value: file_name.to_string(),
                    insert_text: Some(format!(
                        "{}{}",
                        if render_base.ends_with('/') || render_base.is_empty() {
                            ""
                        } else {
                            "/"
                        },
                        file_name
                    )),
                    insert_text_offset: render_base.len(),
                    kind: CompletionItemKind::FOLDER,
                });
            } else if path.is_file() {
                if file_type == FileType::Build {
                    if options.targets {
                        if let Some(targets) = self.query_buildable_targets(
                            &format!(
                                "{render_base}{}",
                                if render_base.ends_with(':') { "" } else { ":" }
                            ),
                            workspace,
                        ) {
                            results.extend(targets.into_iter().map(|target| {
                                StringCompletionResult {
                                    value: target.to_owned(),
                                    insert_text: Some(format!(
                                        "{}{}",
                                        if render_base.ends_with(':') { "" } else { ":" },
                                        target
                                    )),
                                    insert_text_offset: render_base.len(),
                                    kind: CompletionItemKind::PROPERTY,
                                }
                            }));
                        }
                    }
                    continue;
                } else if options.files != FilesystemFileCompletionOptions::None {
                    // Check if it's in the list of allowed extensions. If we have a list, and it
                    // doesn't contain the extension, or the file has no extension, skip this file.
                    if options.files == FilesystemFileCompletionOptions::OnlyLoadable {
                        if file_type != FileType::Library {
                            continue;
                        }
                    }

                    results.push(StringCompletionResult {
                        value: file_name.to_string(),
                        insert_text: Some(format!(
                            "{}{}",
                            if render_base.ends_with(':') || render_base.is_empty() {
                                ""
                            } else {
                                ":"
                            },
                            file_name
                        )),
                        insert_text_offset: render_base.len(),
                        kind: CompletionItemKind::FILE,
                    });
                }
            }
        }

        Ok(())
    }

    fn query_buildable_targets(
        &self,
        module: &str,
        workspace: Option<&BazelWorkspace>,
    ) -> Option<Vec<String>> {
        let workspace = workspace?;

        let output = self.client.query(workspace, &format!("{module}*")).ok()?;

        Some(
            output
                .lines()
                .filter_map(|line| line.strip_prefix(module).map(|str| str.to_owned()))
                .collect(),
        )
    }

    fn get_build_language_proto(&self, uri: &LspUrl) -> anyhow::Result<Vec<u8>> {
        let workspace = self
            .workspace::<PathBuf>(None, uri)?
            .ok_or_else(|| anyhow!("Cannot find workspace"))?;

        self.client.build_language(&workspace)
    }

    /// Returns protos for bazel globals (like int, str, dir; but also e.g. cc_library, alias,
    /// test_suite etc.).
    // TODO: Consider caching this
    fn get_bazel_globals(&self, uri: &LspUrl) -> (builtin::BuildLanguage, builtin::Builtins) {
        let language_proto = self.get_build_language_proto(uri);

        let language_proto = language_proto
            .as_deref()
            .unwrap_or(include_bytes!(env!("DEFAULT_BUILD_LANGUAGE_PB")));

        let language = builtin::BuildLanguage::decode(&language_proto[..]).unwrap();

        // TODO: builtins are also dependent on bazel version, but there is no way to obtain those,
        // see https://github.com/bazel-contrib/vscode-bazel/issues/1.
        let builtins_proto = include_bytes!(env!("BUILTIN_PB"));
        let builtins = builtin::Builtins::decode(&builtins_proto[..]).unwrap();

        (language, builtins)
    }

    fn try_get_environment(&self, uri: &LspUrl) -> anyhow::Result<DocModule> {
        let file_type = FileType::from_lsp_url(uri);
        let (language, builtins) = self.get_bazel_globals(uri);

        let members: SmallMap<_, _> = builtin::build_language_to_doc_members(&language)
            .chain(builtin::builtins_to_doc_members(&builtins, file_type))
            .map(|(name, member)| (name, DocItem::Member(member)))
            .collect();

        Ok(DocModule {
            docs: None,
            members,
        })
    }

    fn get_bazel_globals_names(&self, uri: &LspUrl) -> HashSet<String> {
        let (language, builtins) = self.get_bazel_globals(uri);

        language
            .rule
            .iter()
            .map(|rule| rule.name.clone())
            .chain(builtins.global.iter().map(|global| global.name.clone()))
            .chain(
                builtin::MISSING_GLOBALS
                    .iter()
                    .map(|missing| missing.to_string()),
            )
            .collect()
    }
}

impl<Client: BazelClient> LspContext for BazelContext<Client> {
    fn parse_file_with_contents(&self, uri: &LspUrl, content: String) -> LspEvalResult {
        match uri {
            LspUrl::File(path) => {
                match AstModule::parse(&path.to_string_lossy(), content, &Dialect::Extended) {
                    Ok(ast) => {
                        let diagnostics = self
                            .lint_module(uri, &ast)
                            .into_map(eval_message_to_lsp_diagnostic);
                        LspEvalResult {
                            diagnostics,
                            ast: Some(ast),
                        }
                    }
                    Err(e) => {
                        let diagnostics = vec![eval_message_to_lsp_diagnostic(
                            EvalMessage::from_error(path, &e),
                        )];
                        LspEvalResult {
                            diagnostics,
                            ast: None,
                        }
                    }
                }
            }
            _ => LspEvalResult::default(),
        }
    }

    fn resolve_load(
        &self,
        path: &str,
        current_file: &LspUrl,
        workspace_root: Option<&Path>,
    ) -> anyhow::Result<LspUrl> {
        let label = Label::parse(path)?;
        let workspace = self.workspace(workspace_root, current_file)?;

        let folder = self.resolve_folder(&label, current_file, workspace.as_deref())?;

        // Try the presumed filename first, and check if it exists.
        let presumed_path = folder.join(label.name);
        if presumed_path.exists() {
            return Ok(Url::from_file_path(presumed_path).unwrap().try_into()?);
        }

        // If the presumed filename doesn't exist, try to find a build file from the build system
        // and use that instead.
        for build_file_name in FileType::BUILD_FILE_NAMES {
            let path = folder.join(build_file_name);
            if path.exists() {
                return Ok(Url::from_file_path(path).unwrap().try_into()?);
            }
        }

        Err(ResolveLoadError::TargetNotFound(path.to_owned()).into())
    }

    fn render_as_load(
        &self,
        target: &LspUrl,
        current_file: &LspUrl,
        workspace_root: Option<&Path>,
    ) -> anyhow::Result<String> {
        let workspace = self.workspace(workspace_root, current_file)?;

        match (target, current_file) {
            // Check whether the target and the current file are in the same package.
            (LspUrl::File(target_path), LspUrl::File(current_file_path)) if matches!((target_path.parent(), current_file_path.parent()), (Some(a), Some(b)) if a == b) =>
            {
                // Then just return a relative path.
                let target_filename = target_path.file_name();
                match target_filename {
                    Some(filename) => Ok(format!(":{}", filename.to_string_lossy())),
                    None => Err(RenderLoadError::MissingTargetFilename(target_path.clone()).into()),
                }
            }
            (LspUrl::File(target_path), _) => {
                // Try to find a repository that contains the target, as well as the path to the
                // target relative to the repository root. If we can't find a repository, we'll
                // try to resolve the target relative to the workspace root. If we don't have a
                // workspace root, we'll just use the target path as-is.
                let (repository, target_path) = &workspace
                    .as_deref()
                    .and_then(|ws| ws.get_repository_for_path(target_path))
                    .map(|(repository, target_path)| (Some(repository), target_path))
                    .or_else(|| {
                        workspace_root
                            .and_then(|root| target_path.strip_prefix(root).ok())
                            .map(|path| (None, path))
                    })
                    .unwrap_or((None, target_path));

                let target_filename = target_path.file_name();
                match target_filename {
                    Some(filename) => Ok(format!(
                        "@{}//{}:{}",
                        repository.as_ref().unwrap_or(&Cow::Borrowed("")),
                        target_path
                            .parent()
                            .map(|path| path.to_string_lossy())
                            .unwrap_or_default(),
                        filename.to_string_lossy()
                    )),
                    None => Err(
                        RenderLoadError::MissingTargetFilename(target_path.to_path_buf()).into(),
                    ),
                }
            }
            _ => Err(RenderLoadError::WrongScheme(
                "file://".to_owned(),
                target.clone(),
                current_file.clone(),
            )
            .into()),
        }
    }

    fn resolve_string_literal(
        &self,
        literal: &str,
        current_file: &LspUrl,
        workspace_root: Option<&Path>,
    ) -> anyhow::Result<Option<StringLiteralResult>> {
        self.resolve_load(literal, current_file, workspace_root)
            .map(|url| {
                let original_target_name = Path::new(literal).file_name();
                let path_file_name = url.path().file_name();
                let same_filename = original_target_name == path_file_name;

                Some(StringLiteralResult {
                    url: url.clone(),
                    // If the target name is the same as the original target name, we don't need to
                    // do anything. Otherwise, we need to find the function call in the target file
                    // that has a `name` parameter with the same value as the original target name.
                    location_finder: if same_filename {
                        None
                    } else {
                        match Label::parse(literal) {
                            Err(_) => None,
                            Ok(label) => Some(Box::new(move |ast| {
                                Ok(ast.find_function_call_with_name(&label.name))
                            })),
                        }
                    },
                })
            })
    }

    fn get_load_contents(&self, uri: &LspUrl) -> anyhow::Result<Option<String>> {
        match uri {
            LspUrl::File(path) => match path.is_absolute() {
                true => match fs::read_to_string(path) {
                    Ok(contents) => Ok(Some(contents)),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(e.into()),
                },
                false => Err(ContextError::NotAbsolute(uri.clone()).into()),
            },
            LspUrl::Starlark(_) => Ok(None),
            _ => Err(ContextError::WrongScheme("file://".to_owned(), uri.clone()).into()),
        }
    }

    fn get_environment(&self, uri: &LspUrl) -> DocModule {
        self.try_get_environment(uri).unwrap_or_default()
    }

    fn get_url_for_global_symbol(
        &self,
        _current_file: &LspUrl,
        _symbol: &str,
    ) -> anyhow::Result<Option<LspUrl>> {
        Ok(None)
    }

    fn get_string_completion_options(
        &self,
        document_uri: &LspUrl,
        kind: StringCompletionType,
        current_value: &str,
        workspace_root: Option<&Path>,
    ) -> anyhow::Result<Vec<StringCompletionResult>> {
        let workspace = self.workspace(workspace_root, document_uri)?;

        let offer_repository_names = current_value.is_empty()
            || current_value == "@"
            || (current_value.starts_with('@') && !current_value.contains('/'))
            || (!current_value.contains('/') && !current_value.contains(':'));

        let repo_mapping = workspace
            .as_deref()
            .and_then(|ws| self.repo_mapping_for_file(ws, document_uri).ok());

        let mut names = if offer_repository_names {
            if let Some(workspace) = &workspace {
                let repo_names = match &repo_mapping {
                    Some(repo_mappings) => repo_mappings
                        .keys()
                        .filter(|key| *key != "")
                        .map(|key| Cow::Borrowed(key.deref()))
                        .collect(),
                    None => workspace.get_repository_names(),
                };

                repo_names
                    .into_iter()
                    .map(|name| {
                        let name_with_at = format!("@{}", name);
                        let insert_text = format!("{}//", &name_with_at);

                        StringCompletionResult {
                            value: name_with_at,
                            insert_text: Some(insert_text),
                            insert_text_offset: 0,
                            kind: CompletionItemKind::MODULE,
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        // Complete filenames if we're not in the middle of typing a repository name:
        // "@foo" -> don't complete filenames (still typing repository)
        // "@foo/" -> don't complete filenames (need two separating slashes)
        // "@foo//", "@foo//bar -> complete directories (from `@foo//`)
        // "@foo//bar/baz" -> complete directories (from `@foo//bar`)
        // "@foo//bar:baz" -> complete filenames (from `@foo//bar`), and target names if `kind` is `String`
        // "foo" -> complete directories and filenames (ambiguous, might be a relative path or a repository)
        let complete_directories = (!current_value.starts_with('@')
            || current_value.contains("//"))
            && !current_value.contains(':');
        let complete_filenames =
            // Still typing repository
            (!current_value.starts_with('@') || current_value.contains("//")) &&
            // Explicitly typing directory
            (!current_value.contains('/') || current_value.contains(':'));
        let complete_targets = kind == StringCompletionType::String && complete_filenames;
        if complete_directories || complete_filenames || complete_targets {
            if let Some(completion_root) = if complete_directories && complete_filenames {
                // This must mean we don't have a `/` or `:` separator, so we're completing a relative path.
                // Use the document URI's directory as the base.
                document_uri
                    .path()
                    .parent()
                    .map(FilesystemCompletionRoot::Path)
            } else {
                // Complete from the last `:` or `/` in the current value.
                current_value
                    // NOTE: Can't use `rsplit_once` as we need the value _including_ the value
                    // we're splitting on.
                    .rfind(if complete_directories { '/' } else { ':' })
                    .map(|pos| &current_value[..pos + 1])
                    .map(FilesystemCompletionRoot::String)
            } {
                self.get_filesystem_entries(
                    completion_root,
                    document_uri,
                    workspace.as_deref(),
                    &FilesystemCompletionOptions {
                        directories: complete_directories,
                        files: match (kind, complete_filenames) {
                            (StringCompletionType::LoadPath, _) => {
                                FilesystemFileCompletionOptions::OnlyLoadable
                            }
                            (StringCompletionType::String, true) => {
                                FilesystemFileCompletionOptions::All
                            }
                            (StringCompletionType::String, false) => {
                                FilesystemFileCompletionOptions::None
                            }
                        },
                        targets: complete_targets,
                    },
                    &mut names,
                )?;
            }
        }

        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use lsp_types::{NumberOrString, Url};
    use std::path::PathBuf;

    use lsp_types::CompletionItemKind;
    use serde_json::json;
    use starlark::{
        docs::{DocFunction, DocItem, DocMember, DocModule, DocParam, DocString},
        typing::Ty,
    };
    use starlark_lsp::{
        completion::{StringCompletionResult, StringCompletionType},
        server::{LspContext, LspUrl},
    };

    use crate::test_fixture::TestFixture;

    #[test]
    fn relative_resolve_load_in_external_repository() -> anyhow::Result<()> {
        let fixture = TestFixture::new("simple")?;
        let context = fixture.context()?;

        let url = context.resolve_load(
            "//:foo.bzl",
            &LspUrl::File(fixture.external_dir("foo").join("BUILD")),
            None,
        )?;

        assert_eq!(
            url,
            Url::from_file_path(fixture.external_dir("foo").join("foo.bzl"))
                .unwrap()
                .try_into()?
        );

        Ok(())
    }

    #[test]
    fn absolute_resolve_load_in_external_repository() -> anyhow::Result<()> {
        let fixture = TestFixture::new("simple")?;
        let context = fixture.context()?;

        let url = context.resolve_load(
            "@bar//:bar.bzl",
            &LspUrl::File(fixture.external_dir("foo").join("BUILD")),
            None,
        )?;

        assert_eq!(
            url,
            Url::from_file_path(fixture.external_dir("bar").join("bar.bzl"))
                .unwrap()
                .try_into()?
        );

        Ok(())
    }

    #[test]
    fn external_resolve_load_in_root_workspace() -> anyhow::Result<()> {
        let fixture = TestFixture::new("simple")?;
        let context = fixture.context()?;

        let url = context.resolve_load(
            "@foo//:foo.bzl",
            &LspUrl::File(fixture.workspace_root().join("BUILD")),
            Some(&fixture.workspace_root()),
        )?;

        assert_eq!(
            url,
            Url::from_file_path(fixture.external_dir("foo").join("foo.bzl"))
                .unwrap()
                .try_into()?
        );

        Ok(())
    }

    #[test]
    fn external_resolve_load_in_bzlmod_workspace() -> anyhow::Result<()> {
        let fixture = TestFixture::new("bzlmod")?;
        let context = fixture
            .context_builder()?
            .repo_mapping_json(
                "",
                json!({
                    "": "",
                    "rules_rust": "rules_rust~0.36.2",
                }),
            )?
            .build()?;

        let url = context.resolve_load(
            "@rules_rust//rust:defs.bzl",
            &LspUrl::File(fixture.workspace_root().join("BUILD")),
            Some(&fixture.workspace_root()),
        )?;

        assert_eq!(
            url,
            Url::from_file_path(
                fixture
                    .external_dir("rules_rust~0.36.2")
                    .join("rust")
                    .join("defs.bzl")
            )
            .unwrap()
            .try_into()?
        );

        assert_eq!(context.client.profile.borrow().dump_repo_mapping, 1);

        Ok(())
    }

    #[test]
    fn test_completion_for_repositories_in_root_workspace_with_bzlmod() -> anyhow::Result<()> {
        let fixture = TestFixture::new("bzlmod")?;
        let context = fixture
            .context_builder()?
            .repo_mapping_json(
                "",
                json!({
                    "": "",
                    "rules_rust": "rules_rust~0.36.2",
                }),
            )?
            .build()?;

        let completions = context.get_string_completion_options(
            &LspUrl::File(fixture.workspace_root().join("BUILD")),
            StringCompletionType::String,
            "@rules_ru",
            Some(&fixture.workspace_root()),
        )?;

        assert_eq!(
            completions[0],
            StringCompletionResult {
                value: "@rules_rust".into(),
                insert_text: Some("@rules_rust//".into()),
                insert_text_offset: 0,
                kind: CompletionItemKind::MODULE,
            }
        );

        assert_eq!(context.client.profile.borrow().dump_repo_mapping, 1);

        Ok(())
    }

    #[test]
    fn test_completion_for_packages_in_root_workspace_with_bzlmod() -> anyhow::Result<()> {
        let fixture = TestFixture::new("bzlmod")?;
        let context = fixture
            .context_builder()?
            .repo_mapping_json(
                "",
                json!({
                    "": "",
                    "rules_rust": "rules_rust~0.36.2",
                }),
            )?
            .build()?;

        let completions = context.get_string_completion_options(
            &LspUrl::File(fixture.workspace_root().join("BUILD")),
            StringCompletionType::LoadPath,
            "@rules_rust//",
            Some(&fixture.workspace_root()),
        )?;

        assert_eq!(
            completions[0],
            StringCompletionResult {
                value: "rust".into(),
                insert_text: Some("rust".into()),
                insert_text_offset: "@rules_rust//".len(),
                kind: CompletionItemKind::FOLDER,
            }
        );

        assert_eq!(context.client.profile.borrow().query, 0);
        // TODO: Avoid duplicate dump_repo_mapping calls
        assert_eq!(context.client.profile.borrow().dump_repo_mapping, 2);

        Ok(())
    }

    #[test]
    fn test_completion_for_bare_targets() -> anyhow::Result<()> {
        let fixture = TestFixture::new("simple")?;
        let context = fixture.context()?;

        let completions = context.get_string_completion_options(
            &LspUrl::File(fixture.workspace_root().join("BUILD")),
            StringCompletionType::String,
            "",
            Some(&fixture.workspace_root()),
        )?;

        let completion = completions
            .iter()
            .find(|completion| completion.value == "main.cc")
            .unwrap();

        assert_eq!(
            *completion,
            StringCompletionResult {
                value: "main.cc".into(),
                insert_text: Some("main.cc".into()),
                insert_text_offset: 0,
                kind: CompletionItemKind::FILE,
            }
        );

        Ok(())
    }

    #[test]
    fn test_completion_for_files_in_package() -> anyhow::Result<()> {
        let fixture = TestFixture::new("simple")?;
        let context = fixture.context()?;

        let completions = context.get_string_completion_options(
            &LspUrl::File(fixture.workspace_root().join("BUILD")),
            StringCompletionType::String,
            "//foo:",
            Some(&fixture.workspace_root()),
        )?;

        let completion = completions
            .iter()
            .find(|completion| completion.value == "main.cc")
            .unwrap();

        assert_eq!(
            *completion,
            StringCompletionResult {
                value: "main.cc".into(),
                insert_text: Some("main.cc".into()),
                insert_text_offset: "//foo:".len(),
                kind: CompletionItemKind::FILE,
            }
        );

        Ok(())
    }

    #[test]
    fn test_completion_for_targets_in_package() -> anyhow::Result<()> {
        let fixture = TestFixture::new("simple")?;
        let context = fixture
            .context_builder()?
            .query("//foo:*", "//foo:main\n")
            .build()?;

        let completions = context.get_string_completion_options(
            &LspUrl::File(fixture.workspace_root().join("BUILD")),
            StringCompletionType::String,
            "//foo:",
            Some(&fixture.workspace_root()),
        )?;

        let completion = completions
            .iter()
            .find(|completion| completion.value == "main")
            .unwrap();

        assert_eq!(
            *completion,
            StringCompletionResult {
                value: "main".into(),
                insert_text: Some("main".into()),
                insert_text_offset: "//foo:".len(),
                kind: CompletionItemKind::PROPERTY,
            }
        );

        assert_eq!(context.client.profile.borrow().query, 1);

        Ok(())
    }

    #[test]
    fn test_environment_builtins() -> anyhow::Result<()> {
        let fixture = TestFixture::new("simple")?;
        let context = fixture.context()?;

        fn module_contains(module: &DocModule, value: &str) -> bool {
            module.members.iter().any(|(member, _)| member == value)
        }

        let module = context.get_environment(&LspUrl::File(PathBuf::from("/foo/bar.bzl")));

        assert!(!module_contains(&module, "glob"));
        assert!(module_contains(&module, "range"));

        let module = context.get_environment(&LspUrl::File(PathBuf::from("/foo/bar/BUILD")));

        assert!(module_contains(&module, "glob"));
        assert!(module_contains(&module, "range"));

        Ok(())
    }

    #[test]
    fn test_environment_rules() -> anyhow::Result<()> {
        let fixture = TestFixture::new("simple")?;
        let context = fixture.context()?;

        fn module_contains(module: &DocModule, value: &str) -> bool {
            module.members.iter().any(|(member, _)| member == value)
        }

        let module = context.get_environment(&LspUrl::File(PathBuf::from("/foo/bar.bzl")));

        assert!(module_contains(&module, "cc_library"));

        let module = context.get_environment(&LspUrl::File(PathBuf::from("/foo/bar/BUILD")));

        assert!(module_contains(&module, "cc_library"));

        Ok(())
    }

    fn get_function_doc(file_path: &str, function_name: &str) -> DocFunction {
        let fixture = TestFixture::new("simple").unwrap();
        let context = fixture.context().unwrap();

        let module = context.get_environment(&LspUrl::File(PathBuf::from(file_path)));

        let (_, function) = module
            .members
            .iter()
            .find(|(member, _)| *member == function_name)
            .unwrap();

        match function {
            DocItem::Member(DocMember::Function(f)) => f.clone(),
            _ => panic!(),
        }
    }

    #[test]
    fn test_function_doc_code_block() -> anyhow::Result<()> {
        let doc = get_function_doc("/foo/bar/sample.bzl", "hasattr");

        assert_eq!(
            doc.docs.clone().unwrap().summary,
            "Returns True if the object `x` has an attribute or method of the given `name`, otherwise False. Example:  \n```python\nhasattr(ctx.attr, \"myattr\")\n```"
        );

        Ok(())
    }

    #[test]
    fn test_function_doc_links() -> anyhow::Result<()> {
        // select doc contains both page relative links (#) and absolute links (/).
        let select_doc = get_function_doc("sample.bzl", "select");
        assert_eq!(
            select_doc.docs.clone().unwrap().summary,
            "`select()` is the helper function that makes a rule attribute configurable. See [build encyclopedia](https://bazel.build/reference/be/functions#select) for details."
        );

        assert_eq!(
            select_doc.params.pos_or_named,
            vec![
                DocParam {
                    name: "x".into(),
                    default_value: None,
                    docs: Some(DocString {
                      summary: "A dict that maps configuration conditions to values. Each key is a [Label](https://bazel.build/rules/lib/builtins/Label.html) or a label string that identifies a config\\_setting or constraint\\_value instance. See the [documentation on macros](https://bazel.build/rules/macros#label-resolution) for when to use a Label instead of a string.".into(),
                        details: None,
                    }),
                    typ: Ty::any(),
                },
                DocParam {
                    name: "no_match_error".into(),
                    default_value: Some("''".into()),
                    docs: Some(DocString {
                        summary: "Optional custom error to report if no condition matches.".into(),
                        details: None,
                    }),
                    typ: Ty::any(),
                },
            ]
        );

        // aspect contains absolute link.
        let aspect_doc = get_function_doc("sample.bzl", "aspect");
        assert_eq!(
            aspect_doc.docs.clone().unwrap().summary,
            "Creates a new aspect. The result of this function must be stored in a global value. Please see the [introduction to Aspects](https://bazel.build/rules/aspects) for more details."
        );

        Ok(())
    }

    #[test]
    fn test_function_doc_params() -> anyhow::Result<()> {
        let glob_doc = get_function_doc("/foo/bar/BUILD", "glob");

        assert_eq!(
            glob_doc.params.pos_or_named,
            vec![
                DocParam {
                    name: "include".into(),
                    default_value: Some("[]".into()),
                    docs: Some(DocString {
                        summary: "The list of glob patterns to include.".into(),
                        details: None,
                    }),
                    typ: Ty::any(),
                },
                DocParam {
                    name: "exclude".into(),
                    default_value: Some("[]".into()),
                    docs: Some(DocString {
                        summary: "The list of glob patterns to exclude.".into(),
                        details: None,
                    }),
                    typ: Ty::any(),
                },
                DocParam {
                    name: "exclude_directories".into(),
                    default_value: Some("1".into()),
                    docs: Some(DocString {
                        summary: "A flag whether to exclude directories or not.".into(),
                        details: None,
                    }),
                    typ: Ty::any(),
                },
                DocParam {
                    name: "allow_empty".into(),
                    // TODO: Fix this
                    default_value: Some("unbound".into()),
                    docs: Some(DocString {
                        summary: "Whether we allow glob patterns to match nothing. If `allow_empty` is False, each individual include pattern must match something and also the final result must be non-empty (after the matches of the `exclude` patterns are excluded).".into(),
                        details: None,
                    }),
                    typ: Ty::any(),
                },
            ]
        );

        Ok(())
    }

    #[test]
    fn test_function_doc_args_kwargs() -> anyhow::Result<()> {
        assert_eq!(
            get_function_doc("BUILD", "max").params.args,
            Some(DocParam {
                name: "args".into(),
                default_value: None,
                docs: Some(DocString {
                    summary: "The elements to be checked.".into(),
                    details: None,
                }),
                typ: Ty::any(),
            })
        );

        assert_eq!(
            get_function_doc("BUILD", "dict").params.kwargs,
            Some(DocParam {
                name: "kwargs".into(),
                default_value: None,
                docs: Some(DocString {
                    summary: "Dictionary of additional entries.".into(),
                    details: None,
                }),
                typ: Ty::any(),
            })
        );

        Ok(())
    }

    #[test]
    /// Empty summary in DocString strings break starlark-rust. See #41. Here
    /// we ensure that instead of generating an empty summary, the whole
    /// DocString is None.
    fn no_empty_documentation_is_produced() -> anyhow::Result<()> {
        let fixture = TestFixture::new("simple")?;
        let context = fixture.context()?;

        let module = context.get_environment(&LspUrl::File(PathBuf::from("/foo/bar/defs.bzl")));

        fn validate_doc_item(item: &DocItem) {
            match item {
                DocItem::Module(module) => {
                    validate_doc_string(module.docs.as_ref());
                    for member in module.members.values() {
                        validate_doc_item(member)
                    }
                }
                DocItem::Type(r#type) => {
                    validate_doc_string(r#type.docs.as_ref());
                    for member in r#type.members.values() {
                        validate_doc_member(member);
                    }
                }
                DocItem::Member(member) => {
                    validate_doc_member(&member);
                }
            }
        }

        fn validate_doc_member(member: &DocMember) {
            match member {
                DocMember::Function(function) => {
                    validate_doc_string(function.docs.as_ref());
                    for param in &function.params.pos_or_named {
                        validate_doc_string(param.get_doc_string());
                    }
                }
                DocMember::Property(property) => {
                    validate_doc_string(property.docs.as_ref());
                }
            }
        }

        fn validate_doc_string(doc: Option<&DocString>) {
            if let Some(doc) = doc {
                assert!(!doc.summary.trim().is_empty());
            }
        }

        for item in module.members.values() {
            validate_doc_item(item)
        }

        Ok(())
    }

    #[test]
    fn reports_undefined_global_symbols() -> anyhow::Result<()> {
        let fixture = TestFixture::new("simple")?;
        let context = fixture.context()?;

        let result = context.parse_file_with_contents(
            &LspUrl::File(PathBuf::from("/foo.bzl")),
            "
test_suite(name='my_test_suite');

unknown_global_function(42);

a=int(7);

register_toolchains([':my_toolchain']);
"
            .to_string(),
        );

        assert_eq!(1, result.diagnostics.len());
        assert_eq!(
            "Use of undefined variable `unknown_global_function`",
            result.diagnostics[0].message
        );

        Ok(())
    }

    #[test]
    fn reports_misplaced_load_correctly() -> anyhow::Result<()> {
        let fixture = TestFixture::new("simple")?;
        let context = fixture.context()?;

        let files = [
            ("/foo.bzl", true),
            ("/BUILD", true),
            ("/BUILD.bazel", true),
            ("/WORKSPACE", false),
            ("/WORKSPACE.bazel", false),
        ];

        for (name, expected_lint) in files {
            let result = context.parse_file_with_contents(
                &LspUrl::File(PathBuf::from(name)),
                "
test_suite(name='my_test_suite');

load('foo.bzl', 'bar')
"
                .to_string(),
            );

            let has_lint = result.diagnostics.iter().any(|diagnostic| {
                diagnostic.code == Some(NumberOrString::String("misplaced-load".into()))
            });

            if expected_lint {
                assert!(has_lint, "Expected to have lint in {}", name);
            } else {
                assert!(!has_lint, "Expected to not have lint in {}", name);
            }
        }

        Ok(())
    }
}
