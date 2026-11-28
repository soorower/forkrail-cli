#!/bin/sh
# Stands in for `react-scripts build`: copies the finished site into build/.
set -e
rm -rf build
mkdir -p build
cp -R src/. build/
