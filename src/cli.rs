use std::path::PathBuf;

use clap::{Args, Parser};
use serde::Serialize;

#[derive(Debug, Parser, Serialize, Clone)]
#[command(version, about = "Run jest-lua tests from the command line")]
#[serde(rename_all = "camelCase")]
pub struct Cli {
    /// The path to run jest-companion in. Defaults to the current directory.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Run as a Model Context Protocol (MCP) server over stdio instead of doing a
    /// one-shot test run. Lets MCP clients (like Claude) trigger runs via a `run_tests`
    /// tool. In this mode the runCLI options below act as defaults that each tool call
    /// can override.
    #[arg(long)]
    pub mcp: bool,

    /// Timeout in seconds for the Studio plugin to report something.
    /// In MCP mode, this is how long a `run_tests` call waits for the plugin to pick up
    /// the run before reporting that Studio isn't open.
    #[arg(short, long, default_value_t = 30)]
    pub server_timeout: u64,

    /// (MCP mode) Maximum seconds to wait for a single test run to finish after the
    /// Studio plugin has picked it up.
    #[arg(long, default_value_t = 300)]
    pub run_timeout: u64,

    #[command(flatten, next_help_heading = "runCLI options")]
    pub options: JestOptions,
}

#[derive(Debug, Args, Serialize, Clone)]
#[command(rename_all = "camelCase")]
#[serde(rename_all = "camelCase")]
pub struct JestOptions {
    /// Automatically clear mock calls, instances, contexts and results before every test.
    /// Equivalent to calling jest.clearAllMocks() before each test. This does not remove any mock implementation that may have been provided.
    #[arg(long, verbatim_doc_comment)]
    clear_mocks: Option<bool>,

    /// Use this flag to show full diffs and errors instead of a patch.
    #[arg(long)]
    expand: Option<bool>,

    /// Disables stack trace in test results output.
    #[arg(long)]
    no_stack_trace: Option<bool>,

    /// Changes how jest.spyOn() overwrites methods in the spied object, making it behave like older versions of Jest.
    /// When oldFunctionSpying = true, it will overwrite the spied method with a mock object. (old behaviour)
    /// When oldFunctionSpying = false, it will overwrite the spied method with a regular Lua function. (new behaviour)
    #[arg(long, verbatim_doc_comment)]
    old_function_spying: Option<bool>,

    /// Allows the test suite to pass when no files are found.
    #[arg(long)]
    pass_with_no_tests: Option<bool>,

    /// Automatically reset mock state before every test.
    /// Equivalent to calling jest.resetAllMocks() before each test. This will lead to any mocks having their fake implementations removed but does not restore their initial implementation.
    #[arg(long, verbatim_doc_comment)]
    reset_mocks: Option<bool>,

    /// The glob patterns Jest uses to detect test files.
    #[arg(long, value_delimiter = ',')]
    test_match: Option<Vec<String>>,

    /// Run only tests with a name that matches the regex.
    /// For example, suppose you want to run only tests related to authorization which will have names like "GET /api/posts with auth", then you can use testNamePattern = "auth".
    /// The regex is matched against the full name, which is a combination of the test name and all its surrounding describe blocks.
    #[arg(long, verbatim_doc_comment)]
    test_name_pattern: Option<String>,

    /// An array of regexp pattern strings that are tested against all tests paths before executing the test.
    /// Contrary to testPathPattern, it will only run those tests with a path that does not match with the provided regexp expressions.
    #[arg(long, verbatim_doc_comment)]
    test_path_ignore_patterns: Option<Vec<String>>,

    /// A regexp pattern string that is matched against all tests paths before executing the test.
    #[arg(long)]
    test_path_pattern: Option<Option<String>>,

    /// Default timeout of a test in milliseconds.
    #[arg(long)]
    test_timeout: Option<u32>,

    /// Display individual test results with the test suite hierarchy.
    #[arg(long)]
    pub verbose: Option<bool>,

    /// Use this flag to re-record every snapshot that fails during this test run. Can be used together with a test suite pattern or with testNamePattern to re-record snapshots.
    #[arg(short, long)]
    pub update_snapshot: Option<bool>,
}
