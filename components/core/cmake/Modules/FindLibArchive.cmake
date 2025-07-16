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

# Support static linking via pkg-config if requested
if(CLP_USE_STATIC_LIBS)
    set(ENV{PKG_CONFIG} "pkg-config --static")
    set(PKG_CONFIG_USE_STATIC_LIBS ON)
endif()
pkg_search_module(LibArchive QUIET REQUIRED IMPORTED_TARGET GLOBAL "lib${libarchive_LIBNAME}")

# Use appropriate target if pkg-config found one
if(CLP_USE_STATIC_LIBS AND TARGET PkgConfig::LibArchive)
    # Check if it points to static .a; if not, fallback
    get_target_property(_lib_path PkgConfig::LibArchive IMPORTED_LOCATION)
    if(_lib_path MATCHES "\\.a$")
        set(CLP_LIBARCHIVE PkgConfig::LibArchive)
    else()
        # Fallback to manually imported static target
        add_library(LibArchiveStatic IMPORTED STATIC)
        set_target_properties(LibArchiveStatic PROPERTIES
                IMPORTED_LOCATION "${libarchive_ROOT}/lib/libarchive.a"
                INTERFACE_INCLUDE_DIRECTORIES "${libarchive_ROOT}/include"
        )
        set(CLP_LIBARCHIVE LibArchiveStatic)
    endif()
elseif(TARGET PkgConfig::LibArchive)
    set(CLP_LIBARCHIVE PkgConfig::LibArchive)
else()
    message(FATAL_ERROR "Could not locate usable LibArchive target.")
endif()

