// carrotctl socket - serde json over a unix socket, one dispatch path.
// every keybind action has an ipc twin. `subscribe` turns the connection
// into an ndjson event stream so shells never have to poll.
// unknown command -> error, not "ok".
