// the per-client task pair - receive runs in EventHandling, send in
// PostLayout - plus a supervisor that selects on shutdown and gives a
// stuck client a deadline before the hard kill.
