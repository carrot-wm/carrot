// kdl v2 config. parse errors are fatal at startup and rejected on reload -
// never silently fall back to defaults. reload parses fresh, diffs, applies;
// each key is hot (apply live) or cold (log that a restart is needed).
