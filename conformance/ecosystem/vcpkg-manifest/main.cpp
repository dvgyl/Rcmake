// SPDX-License-Identifier: MIT OR Apache-2.0
#include <boost/version.hpp>
#include <fmt/format.h>
#include <nlohmann/json.hpp>
#include <spdlog/spdlog.h>
#include <zlib.h>

int main()
{
    const auto info = nlohmann::json{{"boost", BOOST_VERSION}, {"zlib", zlibVersion()}};
    spdlog::info("{}", fmt::format("{}", info.dump()));
    return info.empty();
}
