#define DISCORDPP_IMPLEMENTATION
#include "discordpp.h"

#include <atomic>
#include <charconv>
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
#include <unordered_set>
#include <windows.h>

struct CommandQueue {
    std::mutex mutex;
    std::queue<std::string> lines;
};

bool valid_signal_payload(const std::string& payload) {
    if (payload.empty() || payload.size() > 1700) return false;
    for (char c : payload) {
        if (!((c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') ||
              (c >= '0' && c <= '9') || c == '-' || c == '_')) return false;
    }
    return true;
}

int main(int argc, char** argv) {
    const bool machine = argc == 4 && std::string(argv[3]) == "--machine";
    if ((argc != 3 && !machine) ||
        (machine && GetFileType(GetStdHandle(STD_OUTPUT_HANDLE)) != FILE_TYPE_PIPE)) {
        std::cerr << "Usage: wolflink_voice_poc <discord_application_id> <shared_lobby_secret> [--machine (piped stdout only)]\n";
        return 2;
    }
    if (machine) std::cout << std::unitbuf;

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
        std::cerr << "Discord SDK (" << static_cast<int>(severity) << "): " << message << '\n';
    }, discordpp::LoggingSeverity::Warning);

    std::optional<discordpp::Call> call;
    bool lobby_requested = false;
    uint64_t self_id = 0;
    bool ready = false;
    uint64_t active_lobby_id = 0;
    std::unordered_set<uint64_t> seen_participants;
    auto last_signal = std::chrono::steady_clock::time_point{};

    client.SetMessageCreatedCallback([&](uint64_t message_id) {
        if (!machine || !ready || active_lobby_id == 0) return;
        auto message = client.GetMessageHandle(message_id);
        if (!message || message->AuthorId() == self_id ||
            !seen_participants.count(message->AuthorId())) return;
        auto lobby = message->Lobby();
        if (!lobby || lobby->Id() != active_lobby_id) return;
        if (lobby->LinkedChannel()) return;
        const std::string content = message->RawContent();
        if (content.rfind("WL1:", 0) != 0) return;
        const auto separator = content.find(':', 4);
        if (separator == std::string::npos) return;
        uint64_t recipient = 0;
        const char* start = content.data() + 4;
        const char* end = content.data() + separator;
        const auto parsed = std::from_chars(start, end, recipient);
        if (parsed.ec != std::errc{} || parsed.ptr != end) return;
        const std::string payload = content.substr(separator + 1);
        if (recipient == self_id && valid_signal_payload(payload)) {
            std::cout << "SIGNAL " << message->AuthorId() << ' ' << payload << std::endl;
        }
    });

    client.SetStatusChangedCallback([&](auto status, auto error, auto detail) {
        ready = status == discordpp::Client::Status::Ready;
        std::cout << "Discord status: " << discordpp::Client::StatusToString(status) << '\n';
        if (error != discordpp::Client::Error::None) {
            std::cerr << "Discord connection error: " << discordpp::Client::ErrorToString(error)
                      << " (" << detail << ")\n";
        }
        if (!ready) {
            lobby_requested = false;
            active_lobby_id = 0;
            call.reset();
            if (machine) {
                for (auto user_id : seen_participants) std::cout << "LEFT " << user_id << std::endl;
            }
            seen_participants.clear();
            return;
        }
        const auto user = client.GetCurrentUserV2();
        if (!user) {
            std::cerr << "Discord is ready without an authenticated user\n";
            return;
        }
        self_id = user->Id();
        std::cout << "Authenticated Discord user ID: " << self_id << '\n';
        if (machine) {
            std::cout << "SELF " << self_id << std::endl;
        }
        if (lobby_requested) return;
        lobby_requested = true;
        client.CreateOrJoinLobby(lobby_secret, [&](auto result, uint64_t lobby_id) {
            if (!result.Successful()) {
                lobby_requested = false;
                std::cerr << "Lobby join failed: " << result.ToString() << '\n';
                return;
            }
            active_lobby_id = lobby_id;
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
            call->SetParticipantChangedCallback([&](uint64_t user_id, bool added) {
                std::cout << "Voice participant " << user_id
                          << (added ? " joined" : " left") << '\n';
                if (machine && added && user_id != self_id && call) {
                    call->SetParticipantVolume(user_id, 0.0f);
                    if (seen_participants.insert(user_id).second) {
                        std::cout << "PEER " << user_id << std::endl;
                    }
                }
                if (!added && seen_participants.erase(user_id) && machine) {
                    std::cout << "LEFT " << user_id << std::endl;
                }
            });
            std::cout << "Voice call started. Commands: participants | volume <user_id> <0..200> | self-mute on|off | self-deaf on|off | quit\n";
        });
    });

    auto verifier = client.CreateAuthorizationCodeVerifier();
    discordpp::AuthorizationArgs auth_args{};
    auth_args.SetClientId(application_id);
    auth_args.SetScopes(discordpp::Client::GetDefaultCommunicationScopes() + " identify");
    auth_args.SetCodeChallenge(verifier.Challenge());
    std::cout << "Starting Discord authorization; check Discord and your browser for approval.\n";
    client.Authorize(auth_args, [&, verifier](auto result, auto code, auto redirect_uri) {
        if (!result.Successful()) {
            std::cerr << "Discord authorization failed: " << result.ToString() << '\n';
            return;
        }
        std::cout << "Discord authorization approved; exchanging code for token.\n";
        client.GetToken(application_id, code, verifier.Verifier(), redirect_uri,
            [&](auto token_result, auto access_token, auto /*refresh_token*/,
                auto /*token_type*/, auto /*expires_in*/, auto /*scopes*/) {
                if (!token_result.Successful()) {
                    std::cerr << "Discord token exchange failed: " << token_result.ToString() << '\n';
                    return;
                }
                if (machine) {
                    // Rust must spawn this process with stdout piped; never echo this line to a terminal.
                    std::cout << "AUTH " << access_token << std::endl;
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
        if (machine && call && ready) {
            std::unordered_set<uint64_t> current_participants;
            for (auto user_id : call->GetParticipants()) {
                current_participants.insert(user_id);
                if (user_id != self_id && seen_participants.insert(user_id).second) {
                    call->SetParticipantVolume(user_id, 0.0f);
                    std::cout << "PEER " << user_id << std::endl;
                }
            }
            for (auto it = seen_participants.begin(); it != seen_participants.end();) {
                if (!current_participants.count(*it)) {
                    std::cout << "LEFT " << *it << std::endl;
                    it = seen_participants.erase(it);
                } else {
                    ++it;
                }
            }
        }
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
                    if (!machine) {
                        std::cout << "Participant " << user_id << " volume set to "
                                  << call->GetParticipantVolume(user_id) << '\n';
                    }
                } else {
                    std::cerr << "Usage: volume <other_user_id> <0..200>\n";
                }
            } else if (command == "signal" && machine && ready && active_lobby_id != 0) {
                uint64_t recipient = 0;
                std::string recipient_text;
                std::string payload;
                std::string extra;
                stream >> recipient_text >> payload;
                const auto parsed = std::from_chars(recipient_text.data(),
                    recipient_text.data() + recipient_text.size(), recipient);
                if (!recipient_text.empty() && parsed.ec == std::errc{} &&
                    parsed.ptr == recipient_text.data() + recipient_text.size() &&
                    !(stream >> extra) && recipient != 0 && recipient != self_id &&
                    seen_participants.count(recipient) && valid_signal_payload(payload)) {
                    auto lobby = client.GetLobbyHandle(active_lobby_id);
                    if (!lobby || lobby->LinkedChannel()) {
                        std::cerr << "Lobby signaling unavailable for a linked or missing lobby\n";
                    } else if (const auto now = std::chrono::steady_clock::now();
                               now - last_signal < std::chrono::milliseconds(500)) {
                        std::cerr << "Lobby signaling rate limited\n";
                    } else {
                        last_signal = now;
                        client.SendLobbyMessage(active_lobby_id,
                            "WL1:" + std::to_string(recipient) + ':' + payload,
                            [](auto result, uint64_t /*message_id*/) {
                                if (!result.Successful()) {
                                    std::cerr << "Lobby signaling failed: " << result.ToString() << '\n';
                                }
                            });
                    }
                } else {
                    std::cerr << "Invalid lobby signaling command\n";
                }
            } else if (command == "self-mute" && call && !machine) {
                std::string value;
                stream >> value;
                if (value == "on" || value == "off") {
                    call->SetSelfMute(value == "on");
                    std::cout << "Self mute=" << (call->GetSelfMute() ? "on" : "off") << '\n';
                } else {
                    std::cerr << "Usage: self-mute on|off\n";
                }
            } else if (command == "self-deaf" && call && !machine) {
                std::string value;
                stream >> value;
                if (value == "on" || value == "off") {
                    call->SetSelfDeaf(value == "on");
                    std::cout << "Self deaf=" << (call->GetSelfDeaf() ? "on" : "off") << '\n';
                } else {
                    std::cerr << "Usage: self-deaf on|off\n";
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
