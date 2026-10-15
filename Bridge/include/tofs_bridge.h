// tofs_bridge.h — C FFI surface for the 3105 device bridge.
#ifndef TOFS_BRIDGE_H
#define TOFS_BRIDGE_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

// --- logging ---------------------------------------------------------------
typedef void (*tb_log_cb)(void *ctx, const char *msg);
/// Install the global log callback. Call once at launch. Returns 0 on success.
int tb_log_init(tb_log_cb cb, void *ctx);

// --- pairing ---------------------------------------------------------------
typedef struct {
    char *error;
    char *device_name;
    char *device_model;
    char *device_udid;
    char *pairing_file_path;
    char *host_alt_irk_hex;
} TBPairResult;

/// Called when the pairable host is listening: publish the service over
/// Bonjour (NetService) with the given name/port/TXT records.
typedef void (*tb_pair_ready_cb)(void *ctx,
                                 const char *service_id,
                                 uint16_t port,
                                 const char *const *txt_keys,
                                 const char *const *txt_vals,
                                 size_t txt_count);
/// Called if the device asks for a PIN (rare on iOS).
typedef void (*tb_pair_pin_cb)(const char *pin, void *ctx);

/// Run the RPPairing host. BLOCKS until the device pairs or an error occurs —
/// call from a background thread. Returns 0 on success.
int tb_pairing_run_host(const char *bind_addr,
                        uint16_t port,
                        const char *name,
                        const char *model,
                        const char *out_path,
                        const char *host_alt_irk_hex,
                        tb_pair_ready_cb ready_cb,
                        tb_pair_pin_cb pin_cb,
                        void *ctx,
                        TBPairResult *out);
void tb_pairing_result_free(TBPairResult *r);

// --- target hosts ----------------------------------------------------------
int  tb_set_target_host(const char *host);
int  tb_set_target_hosts(const char *const *hosts, size_t count);
void tb_clear_target_hosts(void);

// --- sessions --------------------------------------------------------------
/// Open a tunnel session from a pairing file. Returns 0 + handle on success.
int  tb_open(const char *pairing_path, uint32_t *out_handle, char **out_error);
int  tb_close(uint32_t handle);

// --- info / diagnostics ----------------------------------------------------
int tb_services_json(uint32_t handle, char **out_json, char **out_error);
int tb_list_apps_json(uint32_t handle, char **out_json, char **out_error);
int tb_find_container(uint32_t handle, const char *bundle_id, char **out_path, char **out_error);

// --- file service (container-relative; bundle_id selects the container) ----
int tb_fs_list_json(uint32_t handle, const char *bundle_id, const char *rel_path, char **out_json, char **out_error);
int tb_fs_read(uint32_t handle, const char *bundle_id, const char *rel_path,
               uint8_t **out_buf, size_t *out_len, char **out_error);
int tb_fs_write(uint32_t handle, const char *bundle_id, const char *rel_path,
                const uint8_t *data, size_t len, char **out_error);
int tb_fs_mkdir(uint32_t handle, const char *bundle_id, const char *rel_path, char **out_error);
int tb_fs_remove(uint32_t handle, const char *bundle_id, const char *rel_path, char **out_error);
int tb_fs_rename(uint32_t handle, const char *bundle_id, const char *old_rel, const char *new_rel, char **out_error);

// --- ATC arbitrary-path writes (fallback / escape hatch) --------------------
int tb_atc_write_file(uint32_t handle, const char *abs_path,
                      const uint8_t *data, size_t len, char **out_error);
int tb_atc_write_batch(uint32_t handle, const char *target_dir,
                       const char *const *names, const uint8_t *const *datas,
                       const size_t *lens, size_t count, char **out_error);

// --- self test ---------------------------------------------------------------
/// ATC-write a canary into the app's own container, read it back through the
/// file service, byte-compare, clean up. `own_bundle_id` selects our container.
int tb_self_test_json(uint32_t handle, const char *own_bundle_id, char **out_json, char **out_error);

// --- memory ------------------------------------------------------------------
void tb_string_free(char *p);
void tb_buf_free(uint8_t *p, size_t len);

#ifdef __cplusplus
}
#endif

#endif // TOFS_BRIDGE_H
