# Locate and import LibArchive using pkg-config.
# Note: The default FindLibArchive.cmake distributed with CMake lacks support for static linking.
# To address this, we rely on LibArchive's pkg-config file instead of find_library().

set(libarchive_LIBNAME "archive")
set(LIBARCHIVE_PKGCONFIG_DIR "${libarchive_ROOT}/lib/pkgconfig/")

# Workaround: Patch LibArchive’s pkgconfig file to reflect the correct install prefix.
# Context: The "install-remote-tar" task in yscope-dev-util sets INSTALL_PREFIX during the install
# phase, rather than at configure time. Since the first line of libarchive.pc sets prefix=<prefix>
# and is evaluated during the CMake configuration phase, this discrepancy may cause CLP to fail
# resolving headers or libraries in certain edge cases. This is a bug in "install-remote-tar" that
# we should fix in the future. Currently the workaround modifies libarchive.pc in-place to set
# prefix=${libarchive_ROOT}, ensuring consistency.
set(LIBARCHIVE_PKGCONFIG_FILE "${LIBARCHIVE_PKGCONFIG_DIR}/lib${libarchive_LIBNAME}.pc")
file(READ "${LIBARCHIVE_PKGCONFIG_FILE}" _pc_content)
string(REGEX REPLACE "prefix=/usr/local\n" "prefix=${libarchive_ROOT}\n" _pc_content "${_pc_content}")
file(WRITE "${LIBARCHIVE_PKGCONFIG_FILE}" "${_pc_content}")

# Configure pkg-config to discover and import LibArchive
find_package(PkgConfig)
set(ENV{PKG_CONFIG_PATH} "${LIBARCHIVE_PKGCONFIG_DIR};$ENV{PKG_CONFIG_PATH}")
pkg_search_module(LibArchive QUIET REQUIRED IMPORTED_TARGET GLOBAL "lib${libarchive_LIBNAME}")
