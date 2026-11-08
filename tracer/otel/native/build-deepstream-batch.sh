#!/bin/sh
# Usage: build-deepstream-batch.sh SDK_ROOT OUTPUT [--test]
# SDK_ROOT contains sources/includes and lib. No CUDA code is compiled.
set -eu
sdk=${1:?DeepStream SDK root required}
output=${2:?Output shared library required}
source_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [ "${3:-}" = "--test" ]; then
    set -- "$source_dir/deepstream_batch.c" "$source_dir/deepstream_batch_test.c"
else
    set -- "$source_dir/deepstream_batch.c"
fi
# pkg-config output intentionally expands into compiler arguments.
# shellcheck disable=SC2046
${CC:-cc} -shared -fPIC -O2 -g -Wall -Wextra -Werror \
    -I"$sdk/sources/includes" "$@" -o "$output" \
    -L"$sdk/lib" -Wl,-rpath,"$sdk/lib" -lnvdsgst_meta -lnvds_meta \
    $(pkg-config --cflags --libs gstreamer-1.0 gstreamer-video-1.0)
