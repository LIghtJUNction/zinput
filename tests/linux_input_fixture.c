#include <linux/input.h>
#include <stdio.h>

int main(void) {
    const struct input_event events[] = {
        { .type = EV_KEY, .code = KEY_A, .value = 1 },
        { .type = EV_KEY, .code = KEY_A, .value = 2 },
        { .type = EV_KEY, .code = KEY_A, .value = 0 },
        { .type = EV_SYN, .code = SYN_DROPPED },
        { .type = EV_KEY, .code = KEY_B, .value = 1 },
        { .type = EV_SYN, .code = SYN_REPORT },
        { .type = EV_KEY, .code = KEY_C, .value = 1 },
    };
    return fwrite(events, sizeof events, 1, stdout) == 1 ? 0 : 1;
}
