cmake_minimum_required(VERSION 3.21)
project(wolflink_voice_poc LANGUAGES CXX)

if(NOT DEFINED DISCORD_SDK_ROOT)
    message(FATAL_ERROR "Pass -DDISCORD_SDK_ROOT=<extracted Discord Social SDK directory>")
endif()

file(GLOB_RECURSE DISCORD_HEADERS "${DISCORD_SDK_ROOT}/discordpp.h")
file(GLOB_RECURSE DISCORD_LIBS "${DISCORD_SDK_ROOT}/discord_partner_sdk.lib")
file(GLOB_RECURSE DISCORD_DLLS "${DISCORD_SDK_ROOT}/discord_partner_sdk.dll")

list(LENGTH DISCORD_HEADERS HEADER_COUNT)
list(LENGTH DISCORD_LIBS LIB_COUNT)
list(LENGTH DISCORD_DLLS DLL_COUNT)
if(NOT HEADER_COUNT EQUAL 1 OR NOT LIB_COUNT EQUAL 1 OR NOT DLL_COUNT EQUAL 1)
    message(FATAL_ERROR "Expected exactly one discordpp.h, discord_partner_sdk.lib and discord_partner_sdk.dll under DISCORD_SDK_ROOT")
endif()

list(GET DISCORD_HEADERS 0 DISCORD_HEADER)
list(GET DISCORD_LIBS 0 DISCORD_LIB)
list(GET DISCORD_DLLS 0 DISCORD_DLL)
get_filename_component(DISCORD_INCLUDE "${DISCORD_HEADER}" DIRECTORY)

add_executable(wolflink_voice_poc main.cpp)
target_compile_features(wolflink_voice_poc PRIVATE cxx_std_17)
target_include_directories(wolflink_voice_poc PRIVATE "${DISCORD_INCLUDE}")
target_link_libraries(wolflink_voice_poc PRIVATE "${DISCORD_LIB}")
add_custom_command(TARGET wolflink_voice_poc POST_BUILD
    COMMAND ${CMAKE_COMMAND} -E copy_if_different "${DISCORD_DLL}" "$<TARGET_FILE_DIR:wolflink_voice_poc>")
