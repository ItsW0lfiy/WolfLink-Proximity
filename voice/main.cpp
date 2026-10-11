#define DISCORDPP_IMPLEMENTATION
#include "discordpp.h"

#include <atomic>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <iostream>
#include <memory>
#include <mutex>
#include <optional>
#include <queue>
#include <sstream>
#include <string>
#include <thread>

struct CommandQueue {
    std::mutex mutex;
    std::queue<std::string> lines;
};

int main(int argc, char** argv) {
    if (argc != 3) {
        std::cerr << "Usage: wolflink_voice_poc <discord_application_id> <shared_lobby_secret>\n";
        return 2;
    }

    uint64_t application_id = 0;
    try {
        application_id = std::stoull(argv[1]);
    } catch (...) {
        std::cerr << "Invalid Discord application ID\n";
        return 2;
    }
    const std::string lobby_secret = argv[2];
    if (application_id == 0 || lobby_secret.size() < 16) {
        std::cerr << "Application ID must be nonzero; lobby secret must have at least 16 characters\n";
        return 2;
    }

    discordpp::Client client;
    client.SetApplicationId(application_id);
    client.AddLogCallback([](auto message, auto severity) {
        if (severity == discordpp::LoggingSeverity::Error) {
            std::cerr << "Discord SDK: " << message << '\n';
        }
    }, discordpp::LoggingSeverity::Error);

    std::optional<discordpp::Call> call;
    bool lobby_requested = false;
    uint64_t self_id = 0;
    bool ready = false;

    client.SetStatusChangedCallback([&](auto status, auto error, auto detail) {
        ready = status == discordpp::Client::Status::Ready;
        std::cout << "Discord status: " << discordpp::Client::StatusToString(status) << '\n';
        if (error != discordpp::Client::Error::None) {
            std::cerr << "Discord connection error: " << discordpp::Client::ErrorToString(error)
                      << " (" << detail << ")\n";
        }
        if (!ready) {
            lobby_requested = false;
            call.reset();
            return;
        }
        const auto user = client.GetCurrentUserV2();
        if (!user) {
            std::cerr << "Discord is ready without an authenticated user\n";
            return;
        }
        self_id = user->Id();
        std::cout << "Authenticated Discord user ID: " << self_id << '\n';
        if (lobby_requested) return;
        lobby_requested = true;
        client.CreateOrJoinLobby(lobby_secret, [&](auto result, uint64_t lobby_id) {
            if (!result.Successful()) {
                lobby_requested = false;
                std::cerr << "Lobby join failed: " << result.ToString() << '\n';
                return;
            }
            std::cout << "Joined lobby " << lobby_id << '\n';
            auto started = client.StartCall(lobby_id);
            if (!started) {
                std::cerr << "StartCall returned no call\n";
                return;
            }
            call = std::move(started);
            call->SetStatusChangedCallback([](auto call_status, auto call_error, auto detail) {
                std::cout << "Call status: " << discordpp::Call::StatusToString(call_status) << '\n';
                if (call_error != discordpp::Call::Error::None) {
                    std::cerr << "Call error: " << discordpp::Call::ErrorToString(call_error)
                              << " (" << detail << ")\n";
                }
            });
            call->SetParticipantChangedCallback([](uint64_t user_id, bool added) {
                std::cout << "Voice participant " << user_id
                          << (added ? " joined" : " left") << '\n';
            });
            std::cout << "Voice call started. Commands: participants | volume <user_id> <0..200> | quit\n";
        });
    });

    auto verifier = client.CreateAuthorizationCodeVerifier();
    discordpp::AuthorizationArgs auth_args{};
    auth_args.SetClientId(application_id);
    auth_args.SetScopes(discordpp::Client::GetDefaultCommunicationScopes());
    auth_args.SetCodeChallenge(verifier.Challenge());
    client.Authorize(auth_args, [&, verifier](auto result, auto code, auto redirect_uri) {
        if (!result.Successful()) {
            std::cerr << "Discord authorization failed: " << result.ToString() << '\n';
            return;
        }
        client.GetToken(application_id, code, verifier.Verifier(), redirect_uri,
            [&](auto token_result, auto access_token, auto /*refresh_token*/,
                auto /*token_type*/, auto /*expires_in*/, auto /*scopes*/) {
                if (!token_result.Successful()) {
                    std::cerr << "Discord token exchange failed: " << token_result.ToString() << '\n';
                    return;
                }
                client.UpdateToken(discordpp::AuthorizationTokenType::Bearer,
                    access_token, [&](auto update_result) {
                        if (!update_result.Successful()) {
                            std::cerr << "Discord token update failed: " << update_result.ToString() << '\n';
                            return;
                        }
                        client.Connect();
                    });
            });
    });

    auto commands = std::make_shared<CommandQueue>();
    auto running = std::make_shared<std::atomic<bool>>(true);
    std::thread input_thread([commands, running] {
        std::string line;
        while (*running && std::getline(std::cin, line)) {
            std::lock_guard<std::mutex> lock(commands->mutex);
            commands->lines.push(std::move(line));
        }
    });

    while (*running) {
        discordpp::RunCallbacks();
        std::queue<std::string> pending;
        {
            std::lock_guard<std::mutex> lock(commands->mutex);
            std::swap(pending, commands->lines);
        }
        while (!pending.empty()) {
            std::istringstream stream(pending.front());
            std::string command;
            stream >> command;
            if (command == "quit") {
                *running = false;
            } else if (command == "participants" && call) {
                for (auto user_id : call->GetParticipants()) {
                    if (user_id != self_id) {
                        std::cout << "Participant " << user_id
                                  << " volume=" << call->GetParticipantVolume(user_id) << '\n';
                    }
                }
            } else if (command == "volume" && call) {
                uint64_t user_id = 0;
                float volume = -1.0f;
                if (stream >> user_id >> volume && user_id != self_id &&
                    std::isfinite(volume) && volume >= 0.0f && volume <= 200.0f) {
                    call->SetParticipantVolume(user_id, volume);
                    std::cout << "Participant " << user_id << " volume set to "
                              << call->GetParticipantVolume(user_id) << '\n';
                } else {
                    std::cerr << "Usage: volume <other_user_id> <0..200>\n";
                }
            } else if (!command.empty()) {
                std::cerr << "Unknown command or call not ready\n";
            }
            pending.pop();
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    client.Disconnect();
    // stdin may be blocked in getline; process exit releases it.
    input_thread.detach();
    return 0;
}
