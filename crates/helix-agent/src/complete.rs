//! `helix-agent completions bash|zsh|fish`: completion scripts made from
//! the command table, so they always match the parser. Packages ship
//! them as files (packaging/stage.sh runs this).

use crate::cli::{self, Cmd, Opt};

/// Every command path ("files/search") with its command.
fn paths() -> Vec<(String, &'static Cmd)> {
    let mut out = Vec::new();
    fn walk(prefix: &str, c: &'static Cmd, out: &mut Vec<(String, &'static Cmd)>) {
        let path = if prefix.is_empty() { c.name.to_owned() } else { format!("{prefix}/{}", c.name) };
        out.push((path.clone(), c));
        for s in c.subs {
            walk(&path, s, out);
        }
    }
    for c in cli::commands() {
        walk("", c, &mut out);
    }
    out
}

fn known() -> String {
    paths().iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>().join(" ")
}

fn opt_words(opts: &[Opt]) -> Vec<String> {
    opts.iter().chain(cli::GLOBAL).map(|o| format!("--{}", o.name)).chain(["--help".to_owned()]).collect()
}

/// What may come next after `c`: subcommands, the first argument's
/// choices, options.
fn next_words(c: Option<&'static Cmd>) -> Vec<String> {
    match c {
        None => cli::commands().map(|c| c.name.to_owned()).chain(opt_words(&[])).chain(["--version".to_owned()]).collect(),
        Some(c) if !c.subs.is_empty() => c.subs.iter().map(|s| s.name.to_owned()).chain(["--help".to_owned()]).collect(),
        Some(c) => {
            let mut words: Vec<String> = c.args.first().map(|a| a.choices.iter().map(|s| (*s).to_owned()).collect()).unwrap_or_default();
            if c.name == "help" {
                words.extend(cli::commands().map(|c| c.name.to_owned()));
                words.push("exit-codes".into());
            }
            words.extend(opt_words(c.opts));
            words
        }
    }
}

/// Valued options: (name, choices, takes a path).
fn valued() -> Vec<(&'static str, Vec<&'static str>, bool)> {
    let mut out: Vec<(&'static str, Vec<&'static str>, bool)> = Vec::new();
    let all = paths();
    for o in all.iter().flat_map(|(_, c)| c.opts.iter()).chain(cli::GLOBAL) {
        if o.value.is_none() || out.iter().any(|(n, ..)| *n == o.name) {
            continue;
        }
        out.push((o.name, o.choices.to_vec(), matches!(o.name, "output" | "in")));
    }
    out
}

pub fn bash() -> String {
    let mut s = String::from("# bash completion for helix-agent (made by `helix-agent completions bash`)\n_helix_agent() {\n");
    s.push_str("    local cur=${COMP_WORDS[COMP_CWORD]} prev=${COMP_WORDS[COMP_CWORD-1]} at= w i\n");
    s.push_str(&format!("    local known=\" {} \"\n", known()));
    s.push_str("    for ((i = 1; i < COMP_CWORD; i++)); do\n        w=${COMP_WORDS[i]}\n        [[ $w == -* ]] && continue\n        if [[ $known == *\" ${at:+$at/}$w \"* ]]; then at=${at:+$at/}$w; fi\n    done\n");
    s.push_str("    case $prev in\n");
    for (name, choices, file) in valued() {
        if file {
            s.push_str(&format!("        --{name}) COMPREPLY=($(compgen -f -- \"$cur\")); return ;;\n"));
        } else if choices.is_empty() {
            s.push_str(&format!("        --{name}) return ;;\n"));
        } else {
            s.push_str(&format!("        --{name}) COMPREPLY=($(compgen -W \"{}\" -- \"$cur\")); return ;;\n", choices.join(" ")));
        }
    }
    s.push_str("    esac\n    local words\n    case $at in\n");
    s.push_str(&format!("        \"\") words=\"{}\" ;;\n", next_words(None).join(" ")));
    for (path, c) in paths() {
        s.push_str(&format!("        {path}) words=\"{}\" ;;\n", next_words(Some(c)).join(" ")));
    }
    s.push_str("    esac\n    COMPREPLY=($(compgen -W \"$words\" -- \"$cur\"))\n}\ncomplete -F _helix_agent helix-agent\n");
    s
}

fn zsh_quote(text: &str) -> String {
    text.replace('\'', "'\\''")
}

pub fn zsh() -> String {
    let mut s = String::from("#compdef helix-agent\n# zsh completion for helix-agent (made by `helix-agent completions zsh`)\n_helix_agent() {\n");
    // (Not `path`: zsh ties it to $PATH.)
    s.push_str("    local at= w i prev=${words[CURRENT-1]}\n");
    s.push_str(&format!("    local known=\" {} \"\n", known()));
    s.push_str("    for ((i = 2; i < CURRENT; i++)); do\n        w=${words[i]}\n        [[ $w == -* ]] && continue\n        if [[ $known == *\" ${at:+$at/}$w \"* ]]; then at=${at:+$at/}$w; fi\n    done\n");
    s.push_str("    case $prev in\n");
    for (name, choices, file) in valued() {
        if file {
            s.push_str(&format!("        --{name}) _files; return ;;\n"));
        } else if choices.is_empty() {
            s.push_str(&format!("        --{name}) _message '{}'; return ;;\n", name));
        } else {
            s.push_str(&format!("        --{name}) compadd -- {}; return ;;\n", choices.join(" ")));
        }
    }
    s.push_str("    esac\n    local -a commands\n    case $at in\n");
    let describe = |cmds: &mut dyn Iterator<Item = &'static Cmd>| cmds.map(|c| format!("'{}:{}'", c.name, zsh_quote(c.summary))).collect::<Vec<_>>().join(" ");
    s.push_str(&format!("        \"\") commands=({}) ;;\n", describe(&mut cli::commands())));
    for (path, c) in paths() {
        if !c.subs.is_empty() {
            s.push_str(&format!("        {path}) commands=({}) ;;\n", describe(&mut c.subs.iter())));
        }
    }
    s.push_str("    esac\n    if (( ${#commands} )) && [[ ${words[CURRENT]} != -* ]]; then\n        _describe command commands\n        return\n    fi\n    case $at in\n");
    s.push_str(&format!("        \"\") compadd -- {} ;;\n", next_words(None).join(" ")));
    for (path, c) in paths() {
        if c.subs.is_empty() {
            s.push_str(&format!("        {path}) compadd -- {} ;;\n", next_words(Some(c)).join(" ")));
        }
    }
    s.push_str("    esac\n}\n_helix_agent \"$@\"\n");
    s
}

fn fish_quote(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

pub fn fish() -> String {
    let mut s = String::from("# fish completion for helix-agent (made by `helix-agent completions fish`)\n");
    s.push_str(&format!(
        "function __helix_agent_at\n    set -l known {}\n    set -l at\n    for w in (commandline -opc)[2..-1]\n        string match -q -- '-*' $w; and continue\n        set -l next (string join / -- $at $w)\n        contains -- $next $known; and set at $next\n    end\n    test -n \"$at\"; or set at top\n    test \"$at\" = \"$argv[1]\"\nend\n",
        known()
    ));
    s.push_str("complete -c helix-agent -f\n");
    let mut emit = |path: &str, c: Option<&'static Cmd>| {
        let cond = format!("-n '__helix_agent_at {}'", if path.is_empty() { "top" } else { path });
        let subs: Vec<&'static Cmd> = match c {
            None => cli::commands().collect(),
            Some(c) => c.subs.iter().collect(),
        };
        for sub in subs {
            s.push_str(&format!("complete -c helix-agent {cond} -a {} -d {}\n", sub.name, fish_quote(sub.summary)));
        }
        if let Some(c) = c {
            if let Some(first) = c.args.first().filter(|a| !a.choices.is_empty()) {
                s.push_str(&format!("complete -c helix-agent {cond} -a {}\n", fish_quote(&first.choices.join(" "))));
            }
            for o in c.opts {
                let value = match (o.value, o.choices.is_empty(), matches!(o.name, "output" | "in")) {
                    (None, ..) => String::new(),
                    (Some(_), _, true) => " -r -F".into(),
                    (Some(_), true, false) => " -x".into(),
                    (Some(_), false, false) => format!(" -x -a {}", fish_quote(&o.choices.join(" "))),
                };
                s.push_str(&format!("complete -c helix-agent {cond} -l {}{value} -d {}\n", o.name, fish_quote(o.help)));
            }
        }
    };
    emit("", None);
    for (path, c) in paths() {
        emit(&path, Some(c));
    }
    for o in cli::GLOBAL {
        let value = if o.value.is_some() { " -x" } else { "" };
        s.push_str(&format!("complete -c helix-agent -l {}{value} -d {}\n", o.name, fish_quote(o.help)));
    }
    s.push_str("complete -c helix-agent -s h -l help -d 'Print help'\ncomplete -c helix-agent -s V -l version -d 'Print version'\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scripts packages ship (data/completions) are made by this; after
    /// changing the commands, write them again with
    /// `helix-agent completions bash > data/completions/helix-agent.bash`
    /// (zsh: `_helix-agent`, fish: `helix-agent.fish`).
    #[test]
    fn shipped_scripts_are_current() {
        assert_eq!(include_str!("../../../data/completions/helix-agent.bash"), bash());
        assert_eq!(include_str!("../../../data/completions/_helix-agent"), zsh());
        assert_eq!(include_str!("../../../data/completions/helix-agent.fish"), fish());
    }

    /// Every top-level command is in the man page.
    #[test]
    fn man_page_names_every_command() {
        let man = include_str!("../../../data/man/helix-agent.1");
        for c in cli::commands() {
            let named = man.lines().filter(|l| l.starts_with(".B")).any(|l| l.split(|ch: char| ch.is_whitespace() || ch == '"').any(|w| w == c.name));
            assert!(named, "{} in helix-agent.1", c.name);
        }
    }

    #[test]
    fn scripts_name_every_command() {
        for script in [bash(), zsh(), fish()] {
            for (path, _) in paths() {
                assert!(script.contains(&path), "{path}");
            }
            assert!(script.contains("--level") || script.contains("-l level"));
            assert!(script.contains("read-only"));
        }
    }
}
