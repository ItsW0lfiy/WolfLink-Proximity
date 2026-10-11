cmake_minimum_required(VERSION 3.21)
project(wolflink_voice_poc LANGUAGES CXX)

if(NOT DEFINED DISCORD_SDK_ROOT)
    message(FATAL_ERROR "Pass -DDISCORD_SDK_ROOT=<discord_social_sdk directory>")
endif()

if(NOT WIN32 OR NOT CMAKE_SIZEOF_VOID_P EQUAL 8)
    message(FATAL_ERROR "The voice proof of concept requires Windows x64")
endif()

set(DISCORD_HEADER "${DISCORD_SDK_ROOT}/include/discordpp.h")
set(DISCORD_LIB "${DISCORD_SDK_ROOT}/lib/release/discord_partner_sdk.lib")
set(DISCORD_DLL "${DISCORD_SDK_ROOT}/bin/release/discord_partner_sdk.dll")
foreach(REQUIRED_FILE IN ITEMS "${DISCORD_HEADER}" "${DISCORD_LIB}" "${DISCORD_DLL}")
    if(NOT EXISTS "${REQUIRED_FILE}")
        message(FATAL_ERROR "Discord Social SDK file not found: ${REQUIRED_FILE}")
    endif()
endforeach()
file(GLOB DISCORD_RUNTIME_FILES "${DISCORD_SDK_ROOT}/bin/release/*.dll" "${DISCORD_SDK_ROOT}/bin/release/*.kef")

add_executable(wolflink_voice_poc main.cpp)
target_compile_features(wolflink_voice_poc PRIVATE cxx_std_17)
if(MSVC)
    target_compile_options(wolflink_voice_poc PRIVATE /EHsc)
endif()
target_include_directories(wolflink_voice_poc PRIVATE "${DISCORD_SDK_ROOT}/include")
target_link_libraries(wolflink_voice_poc PRIVATE "${DISCORD_LIB}")
add_custom_command(TARGET wolflink_voice_poc POST_BUILD
    COMMAND ${CMAKE_COMMAND} -E copy_if_different ${DISCORD_RUNTIME_FILES} "$<TARGET_FILE_DIR:wolflink_voice_poc>"
    COMMAND_EXPAND_LISTS)
