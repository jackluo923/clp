# Try to find LibArchive
# NOTE: The FindLibArchive.cmake included with CMake has no support for static libraries.
# Instead we use PkgConfig genereated by LibArchive to import

set(libarchive_LIBNAME "archive")

include(cmake/Modules/FindLibraryDependencies.cmake)
file(REAL_PATH "${CMAKE_SOURCE_DIR}/../../build/deps/core/lib${libarchive_LIBNAME}-install/lib/pkgconfig/"
        libarchive_PKGCONFIG_ABS_PATH)
set(ENV{PKG_CONFIG_PATH} "${libarchive_PKGCONFIG_ABS_PATH};$ENV{PKG_CONFIG_PATH}")

### Run pkg-config
find_package(PkgConfig)
pkg_search_module(LibArchive REQUIRED IMPORTED_TARGET GLOBAL "lib${libarchive_LIBNAME}")

if(LibArchive_USE_STATIC_LIBS)
    # Save current value of CMAKE_FIND_LIBRARY_SUFFIXES
    set(libarchive_ORIG_CMAKE_FIND_LIBRARY_SUFFIXES ${CMAKE_FIND_LIBRARY_SUFFIXES})

    # Temporarily change CMAKE_FIND_LIBRARY_SUFFIXES to static library suffix
    set(CMAKE_FIND_LIBRARY_SUFFIXES .a)
endif()

# Set include directory
find_path(LibArchive_INCLUDE_DIR archive.h
        HINTS ${libarchive_PKGCONF_INCLUDEDIR}
        PATH_SUFFIXES include
        )
