# Locate and import LibArchive using pkg-config.
# Note: The default FindLibArchive.cmake distributed with CMake lacks support for static linking.
# To address this, we rely on LibArchive's pkg-config file instead of find_library().

set(libarchive_LIBNAME "archive")
set(LIBARCHIVE_PKGCONFIG_DIR "${libarchive_ROOT}/lib/pkgconfig/")

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

