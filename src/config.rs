// kdl v2 config. parse errors are fatal at startup and rejected on reload -
// never silently fall back to defaults. reload = parse fresh, diff, apply.
// every key is classed hot (apply live) or cold (log that a restart is
// needed).
