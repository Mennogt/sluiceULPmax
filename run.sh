#!/usr/bin/env bash
# Запуск sluiceULPmax на Linux и macOS.
# Если рядом лежит готовый бинарник — запускает его, иначе собирает через cargo.
set -e
cd "$(dirname "$0")"

if [ -x "./sluiceULPmax" ]; then
    exec ./sluiceULPmax "$@"
fi

if command -v cargo >/dev/null 2>&1; then
    exec cargo run --release -- "$@"
fi

echo
echo "  sluiceULPmax не найден."
echo "  Либо положи рядом готовый бинарник из раздела Releases,"
echo "  либо поставь Rust: https://rustup.rs"
echo
exit 1
