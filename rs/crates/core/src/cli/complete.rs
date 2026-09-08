//! Shell completions: candidate words over the generated `Command` tree, and the three
//! shell shims that call back into `avada __complete <shell> -- <words…>`.
//!
//! `clap_complete` is not available offline, so the shims are hand-written and
//! deliberately tiny: every shell forwards the words typed so far and prints what comes
//! back, one candidate per line. The candidates themselves come from the cached schema,
//! so completion works with no instance running.

use clap::Command;

/// Candidates for the word being typed. `words` are the arguments after `avada`, the
/// last one being the (possibly empty) word under the cursor.
///
/// Rules: descend subcommands through the completed words; a completed flag that takes a
/// value makes the next word one of its possible values (or nothing, when free-form); a
/// current word starting with `-` completes `--long` flags; otherwise subcommand names
/// (positionals are free-form). Results are prefix-filtered, deduplicated and sorted.
pub fn candidates(root: &Command, words: &[String]) -> Vec<String> {
    let (done, cur) = match words.split_last() {
        Some((cur, done)) => (done, cur.as_str()),
        None => (&[][..], ""),
    };
    let mut node = root;
    let mut expecting_value_of: Option<&clap::Arg> = None;
    for w in done {
        if expecting_value_of.take().is_some() {
            // This word was the value of the flag before it.
            continue;
        }
        if let Some(sub) = node.find_subcommand(w) {
            node = sub;
            continue;
        }
        if let Some(rest) = w.strip_prefix("--") {
            if !rest.contains('=') {
                expecting_value_of = node
                    .get_arguments()
                    .find(|a| a.get_long() == Some(rest) && a.get_action().takes_values());
            }
            continue;
        }
        if w.starts_with('-') {
            continue;
        }
        if node.get_positionals().next().is_none() && node.has_subcommands() {
            // A word that is neither a verb here nor a positional: we are off the tree.
            return Vec::new();
        }
        // A positional: free-form, nothing to track.
    }
    let mut out: Vec<String> = if let Some(arg) = expecting_value_of {
        arg.get_possible_values()
            .iter()
            .filter(|v| !v.is_hide_set())
            .map(|v| v.get_name().to_string())
            .collect()
    } else if cur.starts_with('-') {
        node.get_arguments()
            .filter(|a| !a.is_hide_set())
            .filter_map(|a| a.get_long().map(|l| format!("--{l}")))
            .collect()
    } else {
        node.get_subcommands()
            .filter(|c| !c.is_hide_set())
            .map(|c| c.get_name().to_string())
            .collect()
    };
    out.retain(|c| c.starts_with(cur));
    out.sort();
    out.dedup();
    out
}

/// The invocation every shim makes; a test pins it so the shims and `__complete` agree.
pub const COMPLETE_VERB: &str = "__complete";

/// bash: `source <(avada completions bash)` or drop into `~/.bash_completion.d/`.
pub fn bash_shim() -> String {
    format!(
        r#"# avada bash completion — `source <(avada completions bash)`
_avada() {{
    local IFS=$'\n'
    COMPREPLY=($(avada {COMPLETE_VERB} bash -- "${{COMP_WORDS[@]:1:COMP_CWORD}}" 2>/dev/null))
}}
complete -o default -F _avada avada
"#
    )
}

/// zsh: `avada completions zsh > "${fpath[1]}/_avada"` (then `compinit`), or
/// `source <(avada completions zsh)` for the current shell.
pub fn zsh_shim() -> String {
    format!(
        r#"#compdef avada
# avada zsh completion — `avada completions zsh > "${{fpath[1]}}/_avada"`
_avada() {{
    local -a reply
    reply=("${{(@f)$(avada {COMPLETE_VERB} zsh -- "${{words[@]:1:$((CURRENT-1))}}" 2>/dev/null)}}")
    compadd -- "${{reply[@]}}"
}}
if [[ "${{zsh_eval_context[-1]}}" == loadautofunc ]]; then
    _avada "$@"
else
    compdef _avada avada
fi
"#
    )
}

/// fish: `avada completions fish > ~/.config/fish/completions/avada.fish`.
pub fn fish_shim() -> String {
    format!(
        r#"# avada fish completion — `avada completions fish > ~/.config/fish/completions/avada.fish`
function __avada_complete
    set -l words (commandline -opc)
    set -e words[1]
    avada {COMPLETE_VERB} fish -- $words (commandline -ct) 2>/dev/null
end
complete -c avada -f -a '(__avada_complete)'
"#
    )
}

/// The shim for a shell name, or `None` for a shell there is no shim for.
pub fn shim(shell: &str) -> Option<String> {
    match shell {
        "bash" => Some(bash_shim()),
        "zsh" => Some(zsh_shim()),
        "fish" => Some(fish_shim()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::schema_cli::command_tree;
    use crate::cli::schema_cli::tests::sample;

    fn c(words: &[&str]) -> Vec<String> {
        let tree = command_tree(&sample());
        candidates(
            &tree,
            &words.iter().map(|w| w.to_string()).collect::<Vec<_>>(),
        )
    }

    #[test]
    fn top_level_lists_verbs_and_hides_the_internal_one() {
        let top = c(&[""]);
        for v in [
            "health",
            "panes",
            "tokens",
            "queues",
            "schema",
            "state",
            "m",
            "completions",
        ] {
            assert!(top.contains(&v.to_string()), "{v} missing from {top:?}");
        }
        assert!(!top.contains(&COMPLETE_VERB.to_string()));
        assert_eq!(c(&["pa"]), ["panes"]);
        assert_eq!(c(&[]), top, "no words at all is the same as an empty word");
    }

    #[test]
    fn every_depth_including_after_m_and_a_module_id() {
        assert_eq!(c(&["panes", ""]), ["output"]);
        assert_eq!(c(&["queues", "tasks", ""]), ["enqueue"]);
        assert_eq!(c(&["state", ""]), ["tree"]);
        assert_eq!(c(&["m", ""]), ["acme/files", "acme/git"]);
        assert_eq!(c(&["m", "acme/f"]), ["acme/files"]);
        assert_eq!(c(&["m", "acme/files", ""]), ["files"]);
        assert_eq!(c(&["m", "acme/files", "files", ""]), ["blame", "tree"]);
        assert_eq!(c(&["m", "acme/git", ""]), ["log"]);
        // A leaf has no subcommands: positionals are free-form.
        assert!(c(&["panes", "output", ""]).is_empty());
        assert!(c(&["nonsense", ""]).is_empty());
    }

    #[test]
    fn flags_and_enum_values() {
        let flags = c(&["panes", "output", "--"]);
        assert_eq!(flags, ["--help", "--mode", "--tail", "--wait-for-idle"]);
        assert_eq!(c(&["panes", "output", "--w"]), ["--wait-for-idle"]);
        assert_eq!(
            c(&["panes", "output", "p1", "--mode", ""]),
            ["raw", "screen"]
        );
        assert_eq!(c(&["panes", "output", "p1", "--mode", "s"]), ["screen"]);
        // A free-form value: nothing to offer, and the flag after it completes again.
        assert!(c(&["panes", "output", "p1", "--tail", ""]).is_empty());
        assert_eq!(
            c(&["panes", "output", "p1", "--tail", "4", "--m"]),
            ["--mode"]
        );
        // A boolean switch takes no value, so the next word is not its value.
        assert_eq!(
            c(&["panes", "output", "--wait-for-idle", "--m"]),
            ["--mode"]
        );
        // --json is offered on POSTs, --refresh on schema.
        assert!(c(&["tokens", "mint", "--"]).contains(&"--json".to_string()));
        assert!(c(&["schema", "--"]).contains(&"--refresh".to_string()));
        assert_eq!(c(&["completions", ""]), Vec::<String>::new());
        assert_eq!(c(&["completions", "--"]), ["--help"]);
    }

    #[test]
    fn shims_call_back_into_complete() {
        for (shell, shim) in [
            ("bash", bash_shim()),
            ("zsh", zsh_shim()),
            ("fish", fish_shim()),
        ] {
            assert!(
                shim.contains(&format!("avada {COMPLETE_VERB} {shell} --")),
                "{shell} shim does not call {COMPLETE_VERB}:\n{shim}"
            );
            assert_eq!(super::shim(shell).as_deref(), Some(shim.as_str()));
        }
        assert!(bash_shim().contains("complete -o default -F _avada avada"));
        assert!(zsh_shim().starts_with("#compdef avada"));
        assert!(fish_shim().contains("complete -c avada"));
        assert_eq!(shim("powershell"), None);
    }
}
