// SPDX-License-Identifier: MIT OR Apache-2.0
#include <zlib.h>

int main()
{
    return zlibVersion()[0] == '\0';
}
