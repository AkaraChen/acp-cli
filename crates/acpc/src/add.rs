//! `acpc add "opencode acp"`: append an `[[agents]]` entry to config.toml,
//! keeping the rest of the file (comments, order) as it was.

use std::path::Path;

use anyhow::Context;
use anyhow::bail;
use toml_edit::DocumentMut;

use crate::config::Config;

/// Programs that run a package; the agent is named by the package.
const LAUNCHERS: &[&str] = &["npx", "pnpx", "bunx", "uvx", "pipx", "pnpm", "yarn", "bun", "uv", "deno"];
/// Launcher subcommands that come before the package (`pnpm dlx pkg`).
const LAUNCHER_SUBCOMMANDS: &[&str] = &["dlx", "x", "exec", "run", "tool"];

pub struct AddAgent {
    /// One shell-quoted command line, or the command's words.
    pub command: Vec<String>,
    pub slug: Option<String>,
    pub name: Option<String>,
    pub make_default: bool,
}

pub struct Added {
    pub slug: String,
    /// The slug `acpc` starts with when no `--agent` is given.
    pub default: Option<String>,
    pub command_found: bool,
}

/// Add the agent to the config file at `path` (created if missing).
pub fn add_agent(path: &Path, request: AddAgent) -> anyhow::Result<Added> {
    let argv = command_line(&request.command)?;
    let slug = match request.slug {
        Some(slug) => slug,
        None => derive_slug(&argv).context("cannot name this agent from its command; pass --slug")?,
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let (updated, default) = add_to(&text, &argv, &slug, request.name.as_deref(), request.make_default)
        .with_context(|| format!("updating {}", path.display()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // Write next to the file, then rename, so a failure never leaves half a config.
    let staged = path.with_extension("toml.new");
    std::fs::write(&staged, updated).with_context(|| format!("writing {}", staged.display()))?;
    std::fs::rename(&staged, path).with_context(|| format!("writing {}", path.display()))?;
    let command_found = crate::config::find_command(Path::new(&argv[0]), std::env::var_os("PATH").as_deref()).is_some();
    Ok(Added {
        slug,
        default,
        command_found,
    })
}

/// `["opencode acp"]` → `["opencode", "acp"]`; several words are taken as is.
fn command_line(words: &[String]) -> anyhow::Result<Vec<String>> {
    let argv = match words {
        [line] => shlex::split(line).with_context(|| format!("cannot parse `{line}` (unbalanced quotes?)"))?,
        words => words.to_vec(),
    };
    if argv.first().is_none_or(|program| program.trim().is_empty()) {
        bail!("the agent command is empty");
    }
    Ok(argv)
}

/// `opencode acp` → `opencode`; `npx -y @scope/claude-agent-acp@latest` →
/// `claude-agent-acp`.
fn derive_slug(argv: &[String]) -> Option<String> {
    let program = Path::new(&argv[0]).file_name()?.to_str()?;
    let word = if LAUNCHERS.contains(&program) {
        let package = argv[1..]
            .iter()
            .find(|arg| !arg.starts_with('-') && !LAUNCHER_SUBCOMMANDS.contains(&arg.as_str()))?;
        package_name(package)
    } else {
        program
    };
    let slug: String = word
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    (!slug.is_empty()).then_some(slug)
}

/// `npm:@scope/pkg@1.2` → `pkg`; `pkg==1.2` → `pkg`; `git+https://…/pkg.git` → `pkg`.
fn package_name(spec: &str) -> &str {
    let spec = spec
        .strip_prefix("npm:")
        .or_else(|| spec.strip_prefix("jsr:"))
        .unwrap_or(spec);
    let spec = spec.rsplit('/').next().unwrap_or(spec);
    let spec = spec.split(['=', '<', '>', '~']).next().unwrap_or(spec);
    // A version follows an `@` that is not the first character.
    let spec = match spec.char_indices().skip(1).find(|(_, c)| *c == '@') {
        Some((at, _)) => &spec[..at],
        None => spec,
    };
    spec.strip_suffix(".git").unwrap_or(spec)
}

/// The new file text, and the default agent slug afterwards.
fn add_to(
    text: &str,
    argv: &[String],
    slug: &str,
    name: Option<&str>,
    make_default: bool,
) -> anyhow::Result<(String, Option<String>)> {
    let existing: Config = toml::from_str(text).context("the current config does not parse")?;
    if existing.agents.iter().any(|agent| agent.slug == slug) {
        bail!("an agent with slug `{slug}` already exists; pick another with --slug");
    }
    let mut doc: DocumentMut = text.parse().context("the current config does not parse")?;

    let mut table = toml_edit::Table::new();
    table["slug"] = toml_edit::value(slug);
    if let Some(name) = name {
        table["name"] = toml_edit::value(name);
    }
    table["command"] = toml_edit::value(argv[0].as_str());
    if argv.len() > 1 {
        let args: toml_edit::Array = argv[1..].iter().map(String::as_str).collect();
        table["args"] = toml_edit::value(args);
    }
    let agents = doc
        .entry("agents")
        .or_insert_with(|| toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    let Some(agents) = agents.as_array_of_tables_mut() else {
        bail!("`agents` must be written as [[agents]] tables");
    };
    agents.push(table);

    // With several agents and no `agent = …`, plain `acpc` would refuse to
    // guess, so keep the agent it used before unless asked to switch.
    let default = if make_default {
        Some(slug.to_string())
    } else {
        existing.agent.clone().or_else(|| {
            existing
                .agents
                .first()
                .map(|agent| agent.slug.clone())
                .or_else(|| Some(slug.to_string()))
        })
    };
    if default != existing.agent
        && let Some(default) = &default
    {
        doc["agent"] = toml_edit::value(default.as_str());
    }

    let updated = doc.to_string();
    let config: Config = toml::from_str(&updated).context("the new entry does not parse")?;
    config.validate()?;
    Ok((updated, default))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn words(line: &str) -> Vec<String> {
        command_line(&[line.to_string()]).expect("command line")
    }

    #[test]
    fn slugs_name_the_agent_not_the_launcher() {
        for (line, slug) in [
            ("opencode acp", "opencode"),
            ("/usr/local/bin/goose acp", "goose"),
            ("gemini --experimental-acp", "gemini"),
            ("npx -y @agentclientprotocol/claude-agent-acp@latest", "claude-agent-acp"),
            ("pnpm dlx codex-acp@0.3", "codex-acp"),
            ("uvx some_agent==1.2 --acp", "some_agent"),
            ("deno run npm:@scope/My.Agent", "my-agent"),
        ] {
            assert_eq!(derive_slug(&words(line)).as_deref(), Some(slug), "{line}");
        }
    }

    #[test]
    fn quoted_arguments_stay_whole() {
        assert_eq!(words("agent --flag 'a b'"), vec!["agent", "--flag", "a b"]);
        assert!(command_line(&["agent 'open".to_string()]).is_err());
        let several = vec!["npx".to_string(), "-y".to_string(), "pkg".to_string()];
        assert_eq!(command_line(&several).expect("words"), several);
    }

    #[test]
    fn first_agent_becomes_the_default() {
        let (text, default) = add_to("", &words("opencode acp"), "opencode", None, false).expect("add");
        assert_eq!(default.as_deref(), Some("opencode"));
        assert_eq!(
            text,
            "agent = \"opencode\"\n\n[[agents]]\nslug = \"opencode\"\ncommand = \"opencode\"\nargs = [\"acp\"]\n"
        );
    }

    #[test]
    fn adding_keeps_the_file_and_the_previous_default() {
        let before = "# my agents\n\n[[agents]]\nslug = \"claude\" # the main one\ncommand = \"claude-acp\"\n";
        let argv = words("opencode acp");
        let (text, default) = add_to(before, &argv, "opencode", Some("OpenCode"), false).expect("add");
        assert_eq!(default.as_deref(), Some("claude"));
        assert!(text.contains("# my agents") && text.contains("# the main one"), "{text}");
        let config: Config = toml::from_str(&text).expect("parse");
        assert_eq!(config.agent.as_deref(), Some("claude"));
        assert_eq!(config.agents[1].name.as_deref(), Some("OpenCode"));
        assert_eq!(config.agents[1].args, vec!["acp"]);

        let (text, default) = add_to(&text, &words("gemini --experimental-acp"), "gemini", None, true).expect("add");
        assert_eq!(default.as_deref(), Some("gemini"));
        assert_eq!(toml::from_str::<Config>(&text).expect("parse").agent.as_deref(), Some("gemini"));

        let err = add_to(&text, &argv, "opencode", None, false).expect_err("duplicate");
        assert!(err.to_string().contains("already exists"), "{err}");
    }
}
