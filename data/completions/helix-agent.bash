# bash completion for helix-agent (made by `helix-agent completions bash`)
_helix_agent() {
    local cur=${COMP_WORDS[COMP_CWORD]} prev=${COMP_WORDS[COMP_CWORD-1]} at= w i
    local known=" status request state windows apps workspaces monitors notifications files files/recent files/search files/starred media media/list media/play media/pause media/play-pause media/next media/previous media/stop media/seek power power/status wait wait/window wait/closed ask confirm remind remind/add remind/list remind/cancel open launch window workspace workspace/switch toggle set notify clipboard clipboard/get clipboard/types clipboard/set a11y a11y/tree a11y/find a11y/wait a11y/do a11y/press a11y/focus a11y/select a11y/set screenshot input input/key input/type input/click input/move input/scroll input/drag input/sequence log help completions "
    for ((i = 1; i < COMP_CWORD; i++)); do
        w=${COMP_WORDS[i]}
        [[ $w == -* ]] && continue
        if [[ $known == *" ${at:+$at/}$w "* ]]; then at=${at:+$at/}$w; fi
    done
    case $prev in
        --level) COMPREPLY=($(compgen -W "read-only desktop apps screen-and-input" -- "$cur")); return ;;
        --app) return ;;
        --limit) return ;;
        --type) return ;;
        --since) return ;;
        --until) return ;;
        --in) COMPREPLY=($(compgen -f -- "$cur")); return ;;
        --player) return ;;
        --title) return ;;
        --timeout) return ;;
        --choice) return ;;
        --default) return ;;
        --yes) return ;;
        --no) return ;;
        --icon) return ;;
        --urgency) COMPREPLY=($(compgen -W "low normal" -- "$cur")); return ;;
        --output) COMPREPLY=($(compgen -f -- "$cur")); return ;;
        --depth) return ;;
        --node) return ;;
        --max-nodes) return ;;
        --role) return ;;
        --name) return ;;
        --state) return ;;
        --window) return ;;
        --monitor) return ;;
        --area) return ;;
        --button) COMPREPLY=($(compgen -W "left middle right back forward" -- "$cur")); return ;;
        --count) return ;;
        --at) return ;;
        --agent) return ;;
    esac
    local words
    case $at in
        "") words="status request state windows apps workspaces monitors notifications files media power wait ask confirm remind open launch window workspace toggle set notify clipboard a11y screenshot input log help completions --json --text --no-ask --sensitive --timeout --help --version" ;;
        status) words="--json --text --no-ask --sensitive --timeout --help" ;;
        request) words="--level --json --text --no-ask --sensitive --timeout --help" ;;
        state) words="--json --text --no-ask --sensitive --timeout --help" ;;
        windows) words="--app --focused --json --text --no-ask --sensitive --timeout --help" ;;
        apps) words="--running --json --text --no-ask --sensitive --timeout --help" ;;
        workspaces) words="--json --text --no-ask --sensitive --timeout --help" ;;
        monitors) words="--json --text --no-ask --sensitive --timeout --help" ;;
        notifications) words="--limit --json --text --no-ask --sensitive --timeout --help" ;;
        files) words="recent search starred --help" ;;
        files/recent) words="--limit --type --json --text --no-ask --sensitive --timeout --help" ;;
        files/search) words="--since --until --type --in --limit --json --text --no-ask --sensitive --timeout --help" ;;
        files/starred) words="--limit --type --json --text --no-ask --sensitive --timeout --help" ;;
        media) words="list play pause play-pause next previous stop seek --help" ;;
        media/list) words="--json --text --no-ask --sensitive --timeout --help" ;;
        media/play) words="--player --json --text --no-ask --sensitive --timeout --help" ;;
        media/pause) words="--player --json --text --no-ask --sensitive --timeout --help" ;;
        media/play-pause) words="--player --json --text --no-ask --sensitive --timeout --help" ;;
        media/next) words="--player --json --text --no-ask --sensitive --timeout --help" ;;
        media/previous) words="--player --json --text --no-ask --sensitive --timeout --help" ;;
        media/stop) words="--player --json --text --no-ask --sensitive --timeout --help" ;;
        media/seek) words="--player --json --text --no-ask --sensitive --timeout --help" ;;
        power) words="status --help" ;;
        power/status) words="--json --text --no-ask --sensitive --timeout --help" ;;
        wait) words="window closed --help" ;;
        wait/window) words="--app --title --focused --timeout --json --text --no-ask --sensitive --timeout --help" ;;
        wait/closed) words="--timeout --json --text --no-ask --sensitive --timeout --help" ;;
        ask) words="--choice --default --timeout --json --text --no-ask --sensitive --timeout --help" ;;
        confirm) words="--yes --no --timeout --json --text --no-ask --sensitive --timeout --help" ;;
        remind) words="add list cancel --help" ;;
        remind/add) words="--json --text --no-ask --sensitive --timeout --help" ;;
        remind/list) words="--json --text --no-ask --sensitive --timeout --help" ;;
        remind/cancel) words="--json --text --no-ask --sensitive --timeout --help" ;;
        open) words="--app --json --text --no-ask --sensitive --timeout --help" ;;
        launch) words="--json --text --no-ask --sensitive --timeout --help" ;;
        window) words="focus close minimize maximize unmaximize restore fullscreen unfullscreen tile-left tile-right move resize to-workspace --json --text --no-ask --sensitive --timeout --help" ;;
        workspace) words="switch --help" ;;
        workspace/switch) words="--json --text --no-ask --sensitive --timeout --help" ;;
        toggle) words="dark-style do-not-disturb night-light wifi airplane-mode mute --json --text --no-ask --sensitive --timeout --help" ;;
        set) words="volume brightness --json --text --no-ask --sensitive --timeout --help" ;;
        notify) words="--icon --urgency --json --text --no-ask --sensitive --timeout --help" ;;
        clipboard) words="get types set --help" ;;
        clipboard/get) words="--type --output --json --text --no-ask --sensitive --timeout --help" ;;
        clipboard/types) words="--json --text --no-ask --sensitive --timeout --help" ;;
        clipboard/set) words="--type --json --text --no-ask --sensitive --timeout --help" ;;
        a11y) words="tree find wait do press focus select set --help" ;;
        a11y/tree) words="--depth --node --max-nodes --bounds --json --text --no-ask --sensitive --timeout --help" ;;
        a11y/find) words="--role --name --state --limit --json --text --no-ask --sensitive --timeout --help" ;;
        a11y/wait) words="--role --name --state --timeout --json --text --no-ask --sensitive --timeout --help" ;;
        a11y/do) words="--json --text --no-ask --sensitive --timeout --help" ;;
        a11y/press) words="--json --text --no-ask --sensitive --timeout --help" ;;
        a11y/focus) words="--json --text --no-ask --sensitive --timeout --help" ;;
        a11y/select) words="--json --text --no-ask --sensitive --timeout --help" ;;
        a11y/set) words="--number --json --text --no-ask --sensitive --timeout --help" ;;
        screenshot) words="--window --monitor --area --output --json --text --no-ask --sensitive --timeout --help" ;;
        input) words="key type click move scroll drag sequence --help" ;;
        input/key) words="--window --json --text --no-ask --sensitive --timeout --help" ;;
        input/type) words="--window --json --text --no-ask --sensitive --timeout --help" ;;
        input/click) words="--window --button --count --json --text --no-ask --sensitive --timeout --help" ;;
        input/move) words="--window --json --text --no-ask --sensitive --timeout --help" ;;
        input/scroll) words="--window --at --json --text --no-ask --sensitive --timeout --help" ;;
        input/drag) words="--window --button --json --text --no-ask --sensitive --timeout --help" ;;
        input/sequence) words="--window --json --text --no-ask --sensitive --timeout --help" ;;
        log) words="--agent --since --json --text --no-ask --sensitive --timeout --help" ;;
        help) words="status request state windows apps workspaces monitors notifications files media power wait ask confirm remind open launch window workspace toggle set notify clipboard a11y screenshot input log help completions exit-codes --json --text --no-ask --sensitive --timeout --help" ;;
        completions) words="bash zsh fish --json --text --no-ask --sensitive --timeout --help" ;;
    esac
    COMPREPLY=($(compgen -W "$words" -- "$cur"))
}
complete -F _helix_agent helix-agent
