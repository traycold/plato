#! /usr/bin/env bash

set -e

declare -a packages=(zlib bzip2 libpng libjpeg openjpeg jbig2dec freetype2 harfbuzz djvulibre mupdf)

for name in "${@:-${packages[@]}}" ; do
	echo "Building ${name}."
	cd "$name"
	[ -e kobo.patch ] && patch -p 1 < kobo.patch
	./build-kobo.sh
	cd ..
done
