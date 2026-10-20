# Pin the implementation against which fd, mmap and fork paths were audited.
builtins.fetchGit {
  url = "https://github.com/zw3rk/hyper-linux.git";
  rev = "8048a96ba0abc7e2f548da684690bf415c99214e";
}
