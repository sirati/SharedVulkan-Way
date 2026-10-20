#ifndef HL_GUEST_STATUS_H
#define HL_GUEST_STATUS_H
#include <stdint.h>
/* Linux wait status. The host's numeric exit code never implies a signal. */
void hl_status_mark_signal(int linux_signal);
int hl_status_current(int exit_code);
void hl_status_publish(int fd, int linux_status);
/* Called only after a terminal host wait. Consumes and closes the private fd.
 * Missing transport records fall back to explicit host wait-kind conversion. */
int hl_status_collect(int fd, int host_status);
#endif
