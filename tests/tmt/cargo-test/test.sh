#!/bin/bash
set -euxo pipefail

# tmt runs tests from a copy of the repository; TMT_TREE is its root.
cd "${TMT_TREE:-$(dirname "$0")/../../..}"

cargo --version
cargo test --locked
