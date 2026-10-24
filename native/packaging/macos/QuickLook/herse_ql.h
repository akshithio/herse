// The Rust renderer (crates/quicklook), linked in as a static library.
#include <stddef.h>
#include <stdint.h>

uint8_t *herse_ql_render(const char *path, size_t *out_len);
void herse_ql_free(uint8_t *ptr, size_t len);
