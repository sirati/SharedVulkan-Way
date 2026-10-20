#ifndef HL_SCRIPT_H
#define HL_SCRIPT_H
/* Initial CLI programs may be the same interpreter scripts accepted by Linux
 * execve. Preserve the complete wrapper and its one optional shebang argument.
 * Interpreters resolve through guest VFS authority, never host path fallback. */
static int hl_initial_script(const char **load_path, int *argc,
                             const char ***argv) {
    for (int depth = 0; ; depth++) {
        char line[257];
        int fd = open(*load_path, O_RDONLY | O_CLOEXEC);
        if (fd < 0) return -1;
        ssize_t n = read(fd, line, 256);
        int saved = errno;
        close(fd);
        if (n < 0) { errno = saved; return -1; }
        if (n < 2 || line[0] != '#' || line[1] != '!') return 0;
        if (depth == 4) { errno = ELOOP; return -1; }
        char *end = memchr(line, '\n', (size_t)n);
        if (!end) {
            if (n == 256) { errno = ENOEXEC; return -1; }
            end = line + n;
        }
        if (memchr(line, 0, (size_t)(end - line))) {
            errno = ENOEXEC; return -1;
        }
        while (end > line + 2 && (end[-1] == ' ' || end[-1] == '\t')) end--;
        *end = 0;
        char *interp = line + 2;
        while (*interp == ' ' || *interp == '\t') interp++;
        if (!*interp) { errno = ENOEXEC; return -1; }
        char *arg = interp;
        while (*arg && *arg != ' ' && *arg != '\t') arg++;
        if (*arg) {
            *arg++ = 0;
            while (*arg == ' ' || *arg == '\t') arg++;
        }
        int extra = *arg ? 2 : 1;
        if (*argc > 4096 - extra) { errno = E2BIG; return -1; }
        char resolved[LINUX_PATH_MAX];
        if (hl_vfs_mode() == HL_FS_ROOTED) {
            hl_vfs_resolve_t r;
            if (hl_vfs_resolve_at(LINUX_AT_FDCWD, interp, 1, 0, &r) < 0)
                return -1;
            if (r.is_virtual) { errno = ENOENT; return -1; }
            snprintf(resolved, sizeof(resolved), "%s", r.host_path);
        } else {
            snprintf(resolved, sizeof(resolved), "%s", interp);
        }
        char *path = strdup(resolved), *name = strdup(interp);
        char *option = *arg ? strdup(arg) : NULL;
        const char **next = calloc((size_t)*argc + extra + 1, sizeof(*next));
        if (!path || !name || (*arg && !option) || !next) {
            free(path); free(name); free(option); free(next);
            errno = ENOMEM; return -1;
        }
        int at = 0;
        next[at++] = name;
        if (option) next[at++] = option;
        /* The previous argv[0] is the guest script path. Its original
         * arguments retain their order and exact contents. */
        for (int i = 0; i < *argc; i++) next[at++] = (*argv)[i];
        free((void *)*argv);
        free((void *)*load_path);
        *argv = next;
        *argc = at;
        *load_path = path;
    }
}
#endif
