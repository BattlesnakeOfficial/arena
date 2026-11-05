//! Shared game engine for arena and the battlesnake CLI: turns `rules` into
//! games. Everything here is sync, side-effect-free, and free of HTTP: the
//! server (and later the battlesnake CLI) drives it. Proxy routing and frame
//! storage stay in the server.
