#!/bin/sh
# llama-server as it goes into the image: static, CPU with AVX2, no -march=native. Used by the workflows.
#   tools/build-llama.sh <llama.cpp commit> <output file>
set -eu
D=$(mktemp -d)
git -C "$D" init -q
git -C "$D" fetch -q --depth 1 https://github.com/ggml-org/llama.cpp "$1"
git -C "$D" checkout -q FETCH_HEAD
cmake -S "$D" -B "$D/build" -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF -DCMAKE_EXE_LINKER_FLAGS=-static \
  -DGGML_NATIVE=OFF -DGGML_AVX2=ON -DGGML_FMA=ON -DGGML_F16C=ON -DLLAMA_OPENSSL=OFF
cmake --build "$D/build" --target llama-server -j"$(nproc)"
cp "$D/build/bin/llama-server" "$2"
