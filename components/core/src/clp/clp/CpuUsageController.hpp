#ifndef CLP_CPUUSAGECONTROLLER_HPP
#define CLP_CPUUSAGECONTROLLER_HPP

#include <sys/types.h>

#include <chrono>

namespace clp {
/**
 * Class for controlling the CPU usage of a process. It should be running in a
 * separate process.
 * The current implementation has the following limitations:
 * 1. The controller can only define a upper bound for the target process. It
 * does not monitor the CPU usage of the target process.
 * 2. The controller exits if it fails to signal the target process. However,
 * the signal is sent using `pid`, which means if the target process terminates
 * but the original target pid has been reused by other process, the new process
 * with the same pid will be controlled unexpected.
 * 3. The controller cannot control the thread-level behaviour. It is assumed
 * the target process is single-threaded. Otherwise, all threads will be
 * controlled with the upper bound.
 * 4. There is no guarantee that whether the controller process will run before
 * the target monitored process. There is no CPU usage bound guaranteed before
 * controller process gets scheduled to run.
 */
    class CpuUsageController {
    public:
        CpuUsageController(pid_t pid, double ratio);

        [[noreturn]] auto start() -> void;

    private:
        pid_t m_pid;
        std::chrono::milliseconds m_execution_time{0};
        std::chrono::microseconds m_sleep_time{0};
    };
}  // namespace clp

#endif
