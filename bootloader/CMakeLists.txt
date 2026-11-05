# Builds only the second-stage bootloader that is flashed with this firmware
# (build/bootloader/bootloader.bin); the app part is a stub. See README.md.
cmake_minimum_required(VERSION 3.22)
include($ENV{IDF_PATH}/tools/cmake/project.cmake)
idf_build_set_property(MINIMAL_BUILD ON)
project(pxc_bootloader)
