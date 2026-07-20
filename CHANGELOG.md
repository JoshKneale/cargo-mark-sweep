# Changelog

## [0.2.0](https://github.com/JoshKneale/cargo-mark-sweep/compare/v0.1.1...v0.2.0) (2026-07-20)


### Features

* report bytes actually reclaimed, not bytes deleted ([#9](https://github.com/JoshKneale/cargo-mark-sweep/issues/9)) ([e2d1ce4](https://github.com/JoshKneale/cargo-mark-sweep/commit/e2d1ce4223812ee8c44c2fb39c37ef8ab250b379))


### Bug Fixes

* retire configs that keep failing for unrecognised reasons ([#7](https://github.com/JoshKneale/cargo-mark-sweep/issues/7)) ([cc23c2a](https://github.com/JoshKneale/cargo-mark-sweep/commit/cc23c2a9ecac94d435a5d0c87060bb5694fc2e36))
* unwedge the mark set and correct hardlink size accounting ([#6](https://github.com/JoshKneale/cargo-mark-sweep/issues/6)) ([c74e6c1](https://github.com/JoshKneale/cargo-mark-sweep/commit/c74e6c1bd98edde7bc5887d90f758029bfd06433))

## [0.1.1](https://github.com/JoshKneale/cargo-mark-sweep/compare/v0.1.0...v0.1.1) (2026-06-20)


### Bug Fixes

* gate print_state to macOS to avoid dead_code on Linux ([#1](https://github.com/JoshKneale/cargo-mark-sweep/issues/1)) ([2a2f403](https://github.com/JoshKneale/cargo-mark-sweep/commit/2a2f403de6de1dd54611a476611e271437aa81df))
