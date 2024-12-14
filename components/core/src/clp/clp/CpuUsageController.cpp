#include "CpuUsageController.hpp"

#include <csignal>
#include <iostream>
#include <thread>

namespace clp {
    CpuUsageController::CpuUsageController(pid_t pid, double ratio) : m_pid{pid} {
        if (0 >= ratio || 1 <= ratio) {
            std::cerr << "Invalid ratio: " << ratio << "\n";
            exit(-1);
        }

        constexpr std::chrono::duration<double, std::chrono::milliseconds::period> cTimeWindow{50};
        auto const execution_time{cTimeWindow * ratio};
        m_execution_time = std::chrono::milliseconds{
                std::chrono::duration_cast<std::chrono::milliseconds>(execution_time)
        };
        m_sleep_time = std::chrono::milliseconds{
                std::chrono::duration_cast<std::chrono::milliseconds>(cTimeWindow - execution_time)
        };
    }

    auto CpuUsageController::start() -> void {
        while (true) {
            std::this_thread::sleep_for(m_execution_time);
            if (auto const err{kill(m_pid, SIGSTOP)}; 0 != err) {
                std::cerr << "Failed to send `SIGSTOP`. Error code: " << err << "\n";
                exit(-1);
            }
            std::this_thread::sleep_for(m_sleep_time);
            if (auto const err{kill(m_pid, SIGCONT)}; 0 != err) {
                std::cerr << "Failed to send `SIGCONT`. Error code: " << err << "\n";
                exit(-1);
            }
        }
    }
}  // namespace clp
