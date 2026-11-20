//! The command tree (helixdesktop/spec `contracts/agent-access/cli.md`,
//! "Commands"), its parser and its `--help`. One table describes every
//! command: the parser, the help texts and the shell completions all read
//! it.

use helix_i18n::{n_, tr, tr_dyn, trf};

/// A positional argument: `<NAME>`, optional (`[<NAME>]`), and/or
/// repeated (`<NAME>...`).
pub struct Arg {
    pub name: &'static str,
    pub help: &'static str,
    pub required: bool,
    pub many: bool,
    /// The words it can be (for completions and checking); empty: any.
    pub choices: &'static [&'static str],
}

/// An option of a command: `--name` or `--name <VALUE>`.
pub struct Opt {
    pub name: &'static str,
    pub value: Option<&'static str>,
    pub help: &'static str,
    pub many: bool,
    pub choices: &'static [&'static str],
}

/// The levels as the shell names them (`README.md`, "Access levels").
pub const LEVELS: &[&str] = &["read-only", "desktop", "apps", "screen-and-input"];
pub const TOGGLES: &[&str] = &["dark-style", "do-not-disturb", "night-light", "wifi", "airplane-mode", "mute"];
pub const VALUES: &[&str] = &["volume", "brightness"];
pub const WINDOW_ACTIONS: &[&str] = &[
    "focus", "close", "minimize", "maximize", "unmaximize", "restore", "fullscreen", "unfullscreen", "tile-left", "tile-right", "move", "resize", "to-workspace",
];
pub const SHELLS: &[&str] = &["bash", "zsh", "fish"];
/// The types `clipboard set` may set (the shell's list).
pub const SET_TYPES: &[&str] = &["text/plain", "text/plain;charset=utf-8", "text/html", "text/uri-list", "image/png"];

/// A command (or a group of subcommands).
pub struct Cmd {
    pub name: &'static str,
    pub summary: &'static str,
    /// The level it needs (`LEVELS`), and a switch it needs too.
    pub level: Option<&'static str>,
    pub switch: Option<&'static str>,
    pub args: &'static [Arg],
    pub opts: &'static [Opt],
    pub subs: &'static [Cmd],
    /// Which records its JSON output holds (`README.md`, "Records").
    pub records: Option<&'static str>,
    /// The shell has the method behind it (helixdesktop/desktop#146–#151
    /// add the rest).
    pub ready: bool,
    /// A group that also runs alone: the subcommand it then is (`media`
    /// is `media list`).
    pub alone: Option<&'static str>,
}

const fn arg(name: &'static str, help: &'static str) -> Arg {
    Arg { name, help, required: true, many: false, choices: &[] }
}
const fn opt_arg(name: &'static str, help: &'static str) -> Arg {
    Arg { name, help, required: false, many: false, choices: &[] }
}
const fn flag(name: &'static str, help: &'static str) -> Opt {
    Opt { name, value: None, help, many: false, choices: &[] }
}
const fn valued(name: &'static str, value: &'static str, help: &'static str) -> Opt {
    Opt { name, value: Some(value), help, many: false, choices: &[] }
}
const fn cmd(name: &'static str, summary: &'static str, level: Option<&'static str>) -> Cmd {
    Cmd { name, summary, level, switch: None, args: &[], opts: &[], subs: &[], records: None, ready: false, alone: None }
}

const LIMIT: Opt = valued("limit", "N", n_("Show at most N"));
const MIME: Opt = valued("type", "MIME", n_("Only this type, e.g. application/pdf"));
const WAIT_FOR: Opt = valued("timeout", "SECONDS", n_("Give up after this long [default: 60; 0: never]"));
const ASK_FOR: Opt = valued("timeout", "SECONDS", n_("Give up waiting for the answer after this long [default: 300; 0: never]"));
const WINDOW: Arg = arg("WINDOW", n_("A window id from “helix-agent windows”"));
const NODE: Arg = arg("NODE", n_("A node id from “helix-agent a11y tree” or “find”"));
const ROLE: Opt = valued("role", "ROLE", n_("Only controls of this role, e.g. button, text, check box"));
const NAME: Opt = valued("name", "TEXT", n_("Only controls whose name contains TEXT"));
const INPUT_WINDOW: Opt = valued("window", "WINDOW", n_("Into this window, focusing it first (default: the focused window)"));
const X: Arg = arg("X", n_("Across the window from its left edge, in logical pixels"));
const Y: Arg = arg("Y", n_("Down the window from its top edge, in logical pixels"));
/// Pointer buttons by name.
pub const BUTTONS: &[&str] = &["left", "middle", "right", "back", "forward"];
const STATE: Opt = Opt { many: true, ..valued("state", "STATE", n_("Only controls in this state, e.g. focused, checked (repeat for more)")) };
const PLAYER: Opt = valued("player", "ID", n_("This player (an id from “helix-agent media”; default: the active one)"));

/// A `media` command that controls a player.
const fn playback(name: &'static str, summary: &'static str) -> Cmd {
    Cmd { ready: true, opts: &[PLAYER], ..cmd(name, summary, Some("desktop")) }
}

/// The top-level commands, in `--help`'s groups.
pub const GROUPS: &[(&str, &[Cmd])] = &[
    (n_("Status:"), &[
        Cmd { ready: true, ..cmd("status", n_("Show whether agent access is on and what this agent may do"), None) },
        Cmd {
            ready: true,
            opts: &[Opt { choices: LEVELS, ..valued("level", "LEVEL", n_("The level to ask for: read-only, desktop, apps or screen-and-input (default: the level set in Settings)")) }],
            ..cmd("request", n_("Ask the user for access now"), None)
        },
    ]),
    (n_("Read (Read only):"), &[
        Cmd { ready: true, records: Some("State"), ..cmd("state", n_("Focused window, workspace and quick settings"), Some("read-only")) },
        Cmd {
            ready: true,
            records: Some("Window"),
            opts: &[valued("app", "APP_ID", n_("Only this app's windows")), flag("focused", n_("Only the focused window"))],
            ..cmd("windows", n_("Open windows"), Some("read-only"))
        },
        Cmd { ready: true, records: Some("App"), opts: &[flag("running", n_("Only running apps"))], ..cmd("apps", n_("Installed and running apps"), Some("read-only")) },
        Cmd { ready: true, records: Some("Workspace"), ..cmd("workspaces", n_("Workspaces"), Some("read-only")) },
        Cmd { ready: true, records: Some("Monitor"), ..cmd("monitors", n_("Monitors"), Some("read-only")) },
        Cmd { ready: true, records: Some("Notification"), opts: &[LIMIT], ..cmd("notifications", n_("Recent notifications"), Some("read-only")) },
        Cmd {
            subs: &[
                Cmd { ready: true, records: Some("File"), opts: &[LIMIT, MIME], ..cmd("recent", n_("Recently used files"), Some("read-only")) },
                Cmd {
                    ready: true,
                    records: Some("File"),
                    args: &[arg("QUERY", n_("Words in the file's name or contents"))],
                    opts: &[
                        valued("since", "WHEN", n_("Changed at or after WHEN")),
                        valued("until", "WHEN", n_("Changed before WHEN")),
                        MIME,
                        valued("in", "FOLDER", n_("Only in this folder")),
                        LIMIT,
                    ],
                    ..cmd("search", n_("Search for files"), Some("read-only"))
                },
                Cmd { ready: true, records: Some("File"), opts: &[LIMIT, MIME], ..cmd("starred", n_("Starred files (as in Files)"), Some("read-only")) },
            ],
            ready: true,
            ..cmd("files", n_("Recent, starred and found files"), Some("read-only"))
        },
        Cmd {
            subs: &[
                Cmd { ready: true, records: Some("Media player"), ..cmd("list", n_("Media players and what they play (also “media” alone)"), Some("read-only")) },
                playback("play", n_("Play")),
                playback("pause", n_("Pause")),
                playback("play-pause", n_("Play or pause")),
                playback("next", n_("Skip to the next track")),
                playback("previous", n_("Go back to the previous track")),
                playback("stop", n_("Stop playing")),
                Cmd {
                    ready: true,
                    args: &[arg("SECONDS", n_("With a sign (+30, -10): from where it is; without: from the start"))],
                    opts: &[PLAYER],
                    ..cmd("seek", n_("Move in the track"), Some("desktop"))
                },
            ],
            ready: true,
            alone: Some("list"),
            ..cmd("media", n_("Media players: what plays; play, pause and skip"), Some("read-only"))
        },
        Cmd {
            subs: &[Cmd { ready: true, records: Some("Power"), ..cmd("status", n_("Battery and power mode (also “power” alone)"), Some("read-only")) }],
            ready: true,
            alone: Some("status"),
            ..cmd("power", n_("Battery and power mode"), Some("read-only"))
        },
        Cmd {
            subs: &[
                Cmd {
                    ready: true,
                    records: Some("Window"),
                    opts: &[valued("app", "APP_ID", n_("A window of this app")), valued("title", "TEXT", n_("A window whose title contains TEXT")), flag("focused", n_("Only once it has the focus")), WAIT_FOR],
                    ..cmd("window", n_("Wait until a matching window is open"), Some("read-only"))
                },
                Cmd { ready: true, args: &[WINDOW], opts: &[WAIT_FOR], ..cmd("closed", n_("Wait until a window is closed"), Some("read-only")) },
            ],
            ready: true,
            ..cmd("wait", n_("Wait for a window to appear or close"), Some("read-only"))
        },
    ]),
    (n_("Talk to the user (Read only):"), &[
        Cmd {
            ready: true,
            args: &[arg("QUESTION", n_("What to ask (plain text, at most 500 characters)"))],
            opts: &[
                Opt { many: true, ..valued("choice", "TEXT", n_("An answer to offer (2 to 8; repeat for more)")) },
                valued("default", "TEXT", n_("The answer filled in at first, or the choice picked at first")),
                ASK_FOR,
            ],
            ..cmd("ask", n_("Ask the user a question"), Some("read-only"))
        },
        Cmd {
            ready: true,
            args: &[arg("QUESTION", n_("What to ask (plain text, at most 500 characters)"))],
            opts: &[valued("yes", "LABEL", n_("The yes button's label (at most 24 characters)")), valued("no", "LABEL", n_("The no button's label (at most 24 characters)")), ASK_FOR],
            ..cmd("confirm", n_("Ask the user yes or no"), Some("read-only"))
        },
        Cmd {
            ready: true,
            subs: &[
                Cmd { ready: true, args: &[arg("WHEN", n_("When to remind: a date and time, or +10m, +2h…")), arg("TEXT", n_("What to say (at most 500 characters)"))], ..cmd("add", n_("Add a reminder"), Some("read-only")) },
                Cmd { ready: true, records: Some("Reminder"), ..cmd("list", n_("This agent's reminders"), Some("read-only")) },
                Cmd { ready: true, args: &[arg("ID", n_("A reminder id from “helix-agent remind list”"))], ..cmd("cancel", n_("Cancel a reminder"), Some("read-only")) },
            ],
            ..cmd("remind", n_("Reminders for the user"), Some("read-only"))
        },
    ]),
    (n_("Act (Control the desktop):"), &[
        Cmd { ready: true, args: &[arg("URI-OR-PATH", n_("A file, folder or URL"))], opts: &[valued("app", "APP_ID", n_("Open it with this app"))], ..cmd("open", n_("Open a file or URL in its app"), Some("desktop")) },
        Cmd {
            ready: true,
            args: &[arg("APP_ID", n_("The app's id, e.g. org.gnome.TextEditor")), Arg { many: true, ..opt_arg("URI-OR-PATH", n_("Files or URLs to open in it")) }],
            ..cmd("launch", n_("Start an app"), Some("desktop"))
        },
        Cmd {
            ready: true,
            args: &[
                Arg { choices: WINDOW_ACTIONS, ..arg("ACTION", n_("focus, close, minimize, maximize, unmaximize, restore, fullscreen, unfullscreen, tile-left, tile-right, move X Y, resize WIDTH HEIGHT or to-workspace INDEX")) },
                WINDOW,
                Arg { many: true, ..opt_arg("ARGS", n_("The action's numbers")) },
            ],
            ..cmd("window", n_("Focus, move, tile or close a window"), Some("desktop"))
        },
        Cmd {
            ready: true,
            subs: &[Cmd { ready: true, args: &[arg("INDEX", n_("The workspace's number, from 0"))], ..cmd("switch", n_("Switch to a workspace"), Some("desktop")) }],
            ..cmd("workspace", n_("Switch workspaces"), Some("desktop"))
        },
        Cmd {
            ready: true,
            args: &[Arg { choices: TOGGLES, ..arg("NAME", n_("dark-style, do-not-disturb, night-light, wifi, airplane-mode or mute")) }, Arg { choices: &["on", "off"], ..opt_arg("STATE", n_("on or off (default: flip it)")) }],
            ..cmd("toggle", n_("Turn a quick setting on or off"), Some("desktop"))
        },
        Cmd { ready: true, args: &[Arg { choices: VALUES, ..arg("NAME", n_("volume or brightness")) }, arg("VALUE", n_("A percentage"))], ..cmd("set", n_("Set volume or brightness"), Some("desktop")) },
        Cmd {
            ready: true,
            args: &[arg("TITLE", n_("The notification's title")), opt_arg("BODY", n_("More text below it"))],
            opts: &[valued("icon", "ICON", n_("An icon name")), Opt { choices: &["low", "normal"], ..valued("urgency", "URGENCY", n_("low or normal")) }],
            ..cmd("notify", n_("Show the user a notification"), Some("desktop"))
        },
        Cmd {
            ready: true,
            switch: Some("clipboard"),
            subs: &[
                Cmd {
                    ready: true,
                    switch: Some("clipboard"),
                    records: Some("Clipboard"),
                    opts: &[
                        valued("type", "MIME", n_("The type to read, e.g. text/plain, text/html or image/* (default: text, else an image)")),
                        valued("output", "FILE", n_("Write the content to FILE")),
                    ],
                    ..cmd("get", n_("Read the clipboard"), Some("read-only"))
                },
                Cmd { ready: true, switch: Some("clipboard"), ..cmd("types", n_("The types the clipboard offers"), Some("read-only")) },
                Cmd {
                    ready: true,
                    switch: Some("clipboard"),
                    args: &[opt_arg("TEXT", n_("The text (default: read from standard input)"))],
                    opts: &[Opt { choices: SET_TYPES, ..valued("type", "MIME", n_("text/plain (default), text/html, text/uri-list or image/png")) }],
                    ..cmd("set", n_("Set the clipboard"), Some("desktop"))
                },
            ],
            ..cmd("clipboard", n_("Read or set the clipboard (needs Clipboard access)"), Some("read-only"))
        },
    ]),
    (n_("Apps (Control apps):"), &[Cmd {
        ready: true,
        subs: &[
            Cmd {
                ready: true,
                records: Some("Accessible node"),
                args: &[WINDOW],
                opts: &[
                    valued("depth", "N", n_("How many levels down to go (0: the node alone)")),
                    valued("node", "NODE", n_("Start at this node")),
                    valued("max-nodes", "N", n_("Read at most N nodes [default: 1000]")),
                    flag("bounds", n_("Where each control is in its window")),
                ],
                ..cmd("tree", n_("An app window's controls"), Some("apps"))
            },
            Cmd { ready: true, records: Some("Accessible node"), args: &[WINDOW], opts: &[ROLE, NAME, STATE, LIMIT], ..cmd("find", n_("Find controls by role, name or state"), Some("apps")) },
            Cmd { ready: true, records: Some("Accessible node"), args: &[WINDOW], opts: &[ROLE, NAME, STATE, WAIT_FOR], ..cmd("wait", n_("Wait until a matching control is there"), Some("apps")) },
            Cmd { ready: true, args: &[WINDOW, NODE, arg("ACTION", n_("One of the node's actions, e.g. click"))], ..cmd("do", n_("Use a control"), Some("apps")) },
            Cmd { ready: true, args: &[WINDOW, NODE], ..cmd("press", n_("Press a control (its first action)"), Some("apps")) },
            Cmd { ready: true, args: &[WINDOW, NODE], ..cmd("focus", n_("Give a control the keyboard focus"), Some("apps")) },
            Cmd { ready: true, args: &[WINDOW, NODE], ..cmd("select", n_("Select an item in its list or tabs"), Some("apps")) },
            Cmd {
                ready: true,
                args: &[WINDOW, NODE, arg("VALUE", n_("The new text, or number with --number"))],
                opts: &[flag("number", n_("VALUE is a number (sliders, spin buttons)"))],
                ..cmd("set", n_("Change a control's text or value"), Some("apps"))
            },
        ],
        ..cmd("a11y", n_("Read and use an app's controls"), Some("apps"))
    }]),
    (n_("Screen and input (Screen and input):"), &[
        Cmd {
            ready: true,
            records: Some("Image"),
            opts: &[
                valued("window", "WINDOW", n_("Only this window (it may be on another workspace)")),
                valued("monitor", "CONNECTOR", n_("Only this monitor, e.g. eDP-1 (see “helix-agent monitors”)")),
                valued("area", "X,Y,WIDTH,HEIGHT", n_("Only this area: in the window, the monitor or the screen, in logical pixels")),
                valued("output", "FILE", n_("Write the PNG to FILE")),
            ],
            ..cmd("screenshot", n_("Take a screenshot"), Some("screen-and-input"))
        },
        Cmd {
            ready: true,
            subs: &[
                Cmd { ready: true, args: &[Arg { many: true, ..arg("KEYS", n_("Key combinations such as ctrl+s, alt+F4 or Return, pressed in turn")) }], opts: &[INPUT_WINDOW], ..cmd("key", n_("Press keys"), Some("screen-and-input")) },
                Cmd { ready: true, args: &[arg("TEXT", n_("The text (any characters; a line break is Return)"))], opts: &[INPUT_WINDOW], ..cmd("type", n_("Type text"), Some("screen-and-input")) },
                Cmd {
                    ready: true,
                    args: &[X, Y],
                    opts: &[INPUT_WINDOW, Opt { choices: BUTTONS, ..valued("button", "BUTTON", n_("left (default), middle, right, back or forward")) }, valued("count", "N", n_("Click N times (2: a double click; at most 3)"))],
                    ..cmd("click", n_("Click at a point in the window"), Some("screen-and-input"))
                },
                Cmd { ready: true, args: &[X, Y], opts: &[INPUT_WINDOW], ..cmd("move", n_("Move the pointer to a point in the window"), Some("screen-and-input")) },
                Cmd {
                    ready: true,
                    args: &[arg("DX", n_("Wheel steps across (negative: left)")), arg("DY", n_("Wheel steps down (negative: up)"))],
                    opts: &[INPUT_WINDOW, valued("at", "X,Y", n_("Scroll at this point in the window (default: where the pointer is)"))],
                    ..cmd("scroll", n_("Scroll"), Some("screen-and-input"))
                },
                Cmd {
                    ready: true,
                    args: &[arg("X1", n_("From, across")), arg("Y1", n_("From, down")), arg("X2", n_("To, across")), arg("Y2", n_("To, down"))],
                    opts: &[INPUT_WINDOW, Opt { choices: BUTTONS, ..valued("button", "BUTTON", n_("left (default), middle, right, back or forward")) }],
                    ..cmd("drag", n_("Drag from one point to another"), Some("screen-and-input"))
                },
                Cmd {
                    ready: true,
                    args: &[Arg { many: true, ..arg("STEP", n_("move X Y, click [BUTTON] [X Y], double-click [BUTTON] [X Y], mouse-down [BUTTON] [X Y], mouse-up [BUTTON], key KEYS, key-down KEY, key-up KEY, scroll DX DY, type TEXT, wait MS")) }],
                    opts: &[INPUT_WINDOW],
                    ..cmd("sequence", n_("Several steps in one go (keys and buttons held down are let go at the end)"), Some("screen-and-input"))
                },
            ],
            ..cmd("input", n_("Send keys, text, clicks and scrolling"), Some("screen-and-input"))
        },
    ]),
    (n_("Other:"), &[
        Cmd { ready: true, opts: &[valued("agent", "ID", n_("Only this agent")), valued("since", "WHEN", n_("Only entries at or after WHEN"))], ..cmd("log", n_("Show what agents did"), None) },
        Cmd { ready: true, args: &[Arg { many: true, ..opt_arg("COMMAND", n_("A command, or “exit-codes”")) }], ..cmd("help", n_("Help for a command, or \"help exit-codes\""), None) },
        Cmd { ready: true, args: &[Arg { choices: SHELLS, ..arg("SHELL", n_("bash, zsh or fish")) }], ..cmd("completions", n_("Shell completions"), None) },
    ]),
];

/// The global options (`cli.md`, "Usage").
pub const GLOBAL: &[Opt] = &[
    flag("json", n_("JSON output (default when not on a terminal)")),
    flag("text", n_("Readable text (default on a terminal)")),
    flag("no-ask", n_("Never ask the user; fail instead")),
    flag("sensitive", n_("Mark this action sensitive")),
    valued("timeout", "SECONDS", n_("How long to wait for the shell [default: 30]")),
];

pub fn commands() -> impl Iterator<Item = &'static Cmd> {
    GROUPS.iter().flat_map(|(_, cmds)| cmds.iter())
}

/// The command at `path` (`["files", "search"]`).
pub fn find(path: &[&str]) -> Option<&'static Cmd> {
    let (first, rest) = path.split_first()?;
    let mut at = commands().find(|c| c.name == *first)?;
    for name in rest {
        at = at.subs.iter().find(|c| c.name == *name)?;
    }
    Some(at)
}

/// The level's name as Settings shows it.
pub fn level_name(level: &str) -> String {
    match level {
        "read-only" => tr("Read only").to_owned(),
        "desktop" => tr("Control the desktop").to_owned(),
        "apps" => tr("Control apps").to_owned(),
        "screen-and-input" => tr("Screen and input").to_owned(),
        other => other.to_owned(),
    }
}

/// What the command line asked for.
#[derive(Debug, Default, PartialEq)]
pub struct Parsed {
    /// The command's path, e.g. `["files", "search"]`; empty: none given.
    pub path: Vec<&'static str>,
    pub args: Vec<String>,
    /// Options given (name, value; flags have an empty value), in order.
    pub opts: Vec<(&'static str, String)>,
    pub json: Option<bool>,
    pub no_ask: bool,
    pub sensitive: bool,
    /// The global `--timeout` (seconds).
    pub timeout: Option<f64>,
    pub help: bool,
    pub version: bool,
}

impl Parsed {
    pub fn opt(&self, name: &str) -> Option<&str> {
        self.opts.iter().rev().find(|(n, _)| *n == name).map(|(_, v)| v.as_str())
    }

    /// Every value of a repeated option (`ask --choice`).
    pub fn opts_all(&self, name: &str) -> Vec<&str> {
        self.opts.iter().filter(|(n, _)| *n == name).map(|(_, v)| v.as_str()).collect()
    }

    pub fn has(&self, name: &str) -> bool {
        self.opts.iter().any(|(n, _)| *n == name)
    }

    pub fn command(&self) -> Option<&'static Cmd> {
        find(&self.path)
    }
}

/// A usage error's message (exit 2).
pub type UsageError = String;

/// Parse the arguments after the program name.
pub fn parse(argv: &[String]) -> Result<Parsed, UsageError> {
    let mut out = Parsed::default();
    let mut at: Option<&'static Cmd> = None;
    let mut only_positional = false;
    let mut i = 0;
    while i < argv.len() {
        let word = argv[i].as_str();
        i += 1;
        if !only_positional && word == "--" {
            only_positional = true;
            continue;
        }
        if !only_positional && word.starts_with('-') && word.len() > 1 && !is_number(word) {
            let (name, inline) = match word.strip_prefix("--") {
                Some(long) => match long.split_once('=') {
                    Some((n, v)) => (n, Some(v.to_owned())),
                    None => (long, None),
                },
                None => match word {
                    "-h" => ("help", None),
                    "-V" => ("version", None),
                    _ => return Err(trf!("unknown option “{option}”", option = word)),
                },
            };
            match name {
                "help" => out.help = true,
                "version" => out.version = true,
                "json" => out.json = Some(true),
                "text" => out.json = Some(false),
                "no-ask" => out.no_ask = true,
                "sensitive" => out.sensitive = true,
                _ => {
                    // The command's own options first (`wait --timeout` is
                    // the wait's), then the global `--timeout`.
                    let own = at.and_then(|c| c.opts.iter().find(|o| o.name == name));
                    let opt = match own {
                        Some(o) => o,
                        None if name == "timeout" => &GLOBAL[4],
                        None => return Err(trf!("unknown option “--{option}”", option = name)),
                    };
                    let value = match (opt.value, inline) {
                        (None, None) => String::new(),
                        (None, Some(_)) => return Err(trf!("“--{option}” takes no value", option = name)),
                        (Some(_), Some(v)) => v,
                        (Some(_), None) => match argv.get(i) {
                            Some(v) => {
                                i += 1;
                                v.clone()
                            }
                            None => return Err(trf!("“--{option}” needs a value", option = name)),
                        },
                    };
                    if !opt.choices.is_empty() && !opt.choices.contains(&value.as_str()) {
                        return Err(trf!("“{value}” is not one of: {choices}", value = value, choices = opt.choices.join(", ")));
                    }
                    if own.is_none() {
                        out.timeout = Some(seconds(&value)?);
                    } else {
                        if !opt.many && out.has(opt.name) {
                            out.opts.retain(|(n, _)| *n != opt.name);
                        }
                        out.opts.push((opt.name, value));
                    }
                }
            }
            continue;
        }
        // A command or subcommand name, or a positional argument.
        let next = match at {
            None => commands().find(|c| c.name == word),
            Some(c) if out.args.is_empty() => c.subs.iter().find(|s| s.name == word),
            _ => None,
        };
        match next {
            Some(c) => {
                out.path.push(c.name);
                at = Some(c);
            }
            None => match at {
                None => return Err(trf!("unknown command “{command}”", command = word)),
                Some(c) if !c.subs.is_empty() && !out.help => {
                    return Err(trf!("unknown command “{command}” for “{parent}”", command = word, parent = out.path.join(" ")));
                }
                Some(_) => out.args.push(word.to_owned()),
            },
        }
    }
    if out.help || out.version {
        return Ok(out);
    }
    let Some(mut c) = at else { return Ok(out) };
    if let (false, Some(sub)) = (c.subs.is_empty(), c.alone) {
        // A group that runs alone (`media`: `media list`).
        if let Some(s) = c.subs.iter().find(|s| s.name == sub) {
            out.path.push(s.name);
            c = s;
        }
    }
    if !c.subs.is_empty() {
        return Err(trf!("“{command}” needs a subcommand: {subs}", command = out.path.join(" "), subs = c.subs.iter().map(|s| s.name).collect::<Vec<_>>().join(", ")));
    }
    check_args(c, &out.args)?;
    Ok(out)
}

fn is_number(word: &str) -> bool {
    word.parse::<f64>().is_ok()
}

/// A number of seconds (`--timeout`).
pub fn seconds(value: &str) -> Result<f64, UsageError> {
    match value.parse::<f64>() {
        Ok(s) if s >= 0.0 && s.is_finite() => Ok(s),
        _ => Err(trf!("“{value}” is not a number of seconds", value = value)),
    }
}

fn check_args(c: &Cmd, args: &[String]) -> Result<(), UsageError> {
    let required = c.args.iter().filter(|a| a.required).count();
    let many = c.args.iter().any(|a| a.many);
    if args.len() < required {
        let missing = &c.args[args.len().min(c.args.len())..].iter().filter(|a| a.required).map(|a| format!("<{}>", a.name)).collect::<Vec<_>>();
        return Err(trf!("missing argument: {names}", names = missing.join(" ")));
    }
    if !many && args.len() > c.args.len() {
        return Err(trf!("unexpected argument “{argument}”", argument = args[c.args.len()]));
    }
    for (a, value) in c.args.iter().zip(args) {
        if !a.choices.is_empty() && !a.choices.contains(&value.as_str()) {
            return Err(trf!("“{value}” is not one of: {choices}", value = value, choices = a.choices.join(", ")));
        }
    }
    Ok(())
}

fn arg_usage(a: &Arg) -> String {
    let base = format!("<{}>{}", a.name, if a.many { "..." } else { "" });
    if a.required { base } else { format!("[{base}]") }
}

fn opt_usage(o: &Opt) -> String {
    match o.value {
        Some(v) => format!("--{} <{v}>", o.name),
        None => format!("--{}", o.name),
    }
}

/// Rows of `name  help`, the names padded to one width.
fn rows(out: &mut String, rows: &[(String, String)]) {
    let width = rows.iter().map(|(n, _)| n.chars().count()).max().unwrap_or(0);
    for (name, help) in rows {
        let pad = width - name.chars().count();
        out.push_str(&format!("  {name}{}  {help}\n", " ".repeat(pad)));
    }
}

fn global_rows() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = GLOBAL.iter().map(|o| (opt_usage(o), tr_dyn(o.help))).collect();
    out.push(("-h, --help".into(), tr("Print help").into()));
    out.push(("-V, --version".into(), tr("Print version").into()));
    out
}

/// `helix-agent --help`.
pub fn main_help() -> String {
    let mut out = format!("{}\n\n{} helix-agent [OPTIONS] <COMMAND>\n\n", tr("Let an agent read and act on your Helix session."), tr("Usage:"));
    let width = commands().map(|c| c.name.len()).max().unwrap_or(0);
    for (title, cmds) in GROUPS {
        out.push_str(&tr_dyn(title));
        out.push('\n');
        for c in *cmds {
            out.push_str(&format!("  {:width$}  {}\n", c.name, tr_dyn(c.summary)));
        }
    }
    out.push('\n');
    out.push_str(tr("Options:"));
    out.push('\n');
    rows(&mut out, &global_rows());
    out.push('\n');
    out.push_str(tr("Agent access is controlled in Settings › Privacy & Security › Agent Access."));
    out.push('\n');
    out
}

/// A command's `--help` (`path` names it).
pub fn command_help(path: &[&str]) -> Option<String> {
    let c = find(path)?;
    let full = path.join(" ");
    let mut out = format!("{}\n\n", tr_dyn(c.summary));
    let mut usage = format!("{} helix-agent {full}", tr("Usage:"));
    if !c.subs.is_empty() {
        usage.push_str(if c.alone.is_some() { " [<COMMAND>]" } else { " <COMMAND>" });
    }
    if !c.opts.is_empty() {
        usage.push_str(" [OPTIONS]");
    }
    for a in c.args {
        usage.push(' ');
        usage.push_str(&arg_usage(a));
    }
    out.push_str(&usage);
    out.push('\n');
    if let Some(level) = c.level {
        let needs = match c.switch {
            Some(_) => trf!("Needs: the “{level}” access level and Clipboard access", level = level_name(level)),
            None => trf!("Needs: the “{level}” access level", level = level_name(level)),
        };
        out.push_str(&needs);
        out.push('\n');
    }
    if !c.subs.is_empty() {
        out.push('\n');
        out.push_str(tr("Commands:"));
        out.push('\n');
        rows(&mut out, &c.subs.iter().map(|s| (s.name.to_owned(), tr_dyn(s.summary))).collect::<Vec<_>>());
    }
    if !c.args.is_empty() {
        out.push('\n');
        out.push_str(tr("Arguments:"));
        out.push('\n');
        rows(&mut out, &c.args.iter().map(|a| (arg_usage(a), tr_dyn(a.help))).collect::<Vec<_>>());
    }
    out.push('\n');
    out.push_str(tr("Options:"));
    out.push('\n');
    let mut opts: Vec<(String, String)> = c.opts.iter().map(|o| (opt_usage(o), tr_dyn(o.help))).collect();
    let own_timeout = c.opts.iter().any(|o| o.name == "timeout");
    opts.extend(global_rows().into_iter().filter(|(n, _)| !(own_timeout && n.starts_with("--timeout"))));
    rows(&mut out, &opts);
    if let Some(record) = c.records {
        out.push('\n');
        out.push_str(&trf!("JSON output: {record} records (helix-agent(1), “RECORDS”).", record = record));
        out.push('\n');
    }
    if !c.ready {
        out.push('\n');
        out.push_str(tr("This Helix session does not support this command yet."));
        out.push('\n');
    }
    Some(out)
}

/// `helix-agent help exit-codes`.
pub fn exit_codes_help() -> String {
    let mut out = format!("{}\n\n", tr("Exit codes:"));
    let list: Vec<(String, String)> = crate::errors::CODES.iter().map(|(code, exit)| (format!("{exit:>2}  {code}"), crate::errors::describe(code))).collect();
    rows(&mut out, &list);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(line: &str) -> Result<Parsed, UsageError> {
        parse(&line.split_whitespace().map(String::from).collect::<Vec<_>>())
    }

    #[test]
    fn commands_and_options() {
        let parsed = p("--json files search report --since yesterday --limit 5").unwrap();
        assert_eq!(parsed.path, ["files", "search"]);
        assert_eq!(parsed.args, ["report"]);
        assert_eq!(parsed.opt("since"), Some("yesterday"));
        assert_eq!(parsed.opt("limit"), Some("5"));
        assert_eq!(parsed.json, Some(true));
        // Global options also after the command.
        let parsed = p("status --text --no-ask").unwrap();
        assert_eq!((parsed.json, parsed.no_ask), (Some(false), true));
    }

    #[test]
    fn timeouts() {
        // `wait`'s own --timeout; the global one elsewhere.
        let parsed = p("wait window --app org.gnome.TextEditor --timeout 5").unwrap();
        assert_eq!((parsed.opt("timeout"), parsed.timeout), (Some("5"), None));
        let parsed = p("--timeout 0 status").unwrap();
        assert_eq!(parsed.timeout, Some(0.0));
        assert!(p("--timeout soon status").is_err());
    }

    #[test]
    fn usage_errors() {
        assert!(p("frobnicate").is_err());
        assert!(p("files").is_err(), "a subcommand is needed");
        assert!(p("files search").is_err(), "QUERY is missing");
        assert!(p("status extra").is_err());
        assert!(p("request --level everything").is_err());
        assert!(p("toggle wifi maybe").is_err());
        assert!(p("status --bogus").is_err());
        assert!(p("completions tcsh").is_err());
        assert!(p("media rewind").is_err());
        assert!(p("media seek").is_err(), "SECONDS is missing");
        assert!(p("media --player vlc").is_err(), "--player is the controls'");
        assert!(p("power mode").is_err(), "not in this session's commands");
    }

    #[test]
    fn groups_that_run_alone() {
        assert_eq!(p("media").unwrap().path, ["media", "list"]);
        assert_eq!(p("media list").unwrap().path, ["media", "list"]);
        assert_eq!(p("power").unwrap().path, ["power", "status"]);
        let parsed = p("media play-pause --player vlc").unwrap();
        assert_eq!((parsed.path.clone(), parsed.opt("player")), (vec!["media", "play-pause"], Some("vlc")));
        // Signed numbers are arguments.
        assert_eq!(p("media seek -10").unwrap().args, ["-10"]);
        assert_eq!(p("media seek +30").unwrap().args, ["+30"]);
        assert!(p("media --help").unwrap().help);
        assert!(command_help(&["media"]).unwrap().contains("Usage: helix-agent media [<COMMAND>]"));
        assert!(command_help(&["media", "next"]).unwrap().contains("Needs: the “Control the desktop” access level"));
        assert!(command_help(&["media", "list"]).unwrap().contains("Media player records"));
    }

    #[test]
    fn repeated_and_negative() {
        let parsed = p("ask Which? --choice A --choice B").unwrap();
        assert_eq!(parsed.opts_all("choice"), ["A", "B"]);
        // Negative numbers are arguments, not options.
        let parsed = p("input scroll 0 -120").unwrap();
        assert_eq!(parsed.args, ["0", "-120"]);
        let parsed = p("launch org.gnome.Nautilus a b c").unwrap();
        assert_eq!(parsed.args.len(), 4);
    }

    #[test]
    fn help_everywhere() {
        assert!(p("--help").unwrap().help);
        assert!(p("files --help").unwrap().help);
        assert!(p("files search -h").unwrap().help);
        let main = main_help();
        for c in commands() {
            assert!(main.contains(&format!("  {}", c.name)), "{} in the main help", c.name);
            let help = command_help(&[c.name]).unwrap();
            assert!(help.contains("Usage: helix-agent"), "{}", c.name);
            for s in c.subs {
                let help = command_help(&[c.name, s.name]).unwrap();
                assert!(help.contains(&format!("helix-agent {} {}", c.name, s.name)));
            }
        }
        assert!(command_help(&["files", "search"]).unwrap().contains("Needs: the “Read only” access level"));
        assert!(command_help(&["clipboard", "set"]).unwrap().contains("Clipboard access"));
        assert!(command_help(&["clipboard", "types"]).unwrap().contains("Needs: the “Read only” access level and Clipboard access"));
        assert!(p("clipboard set --type image/gif").is_err(), "only the types the shell sets");
        assert_eq!(p("clipboard get --type image/* --output a.png").unwrap().opt("output"), Some("a.png"));
    }
}
