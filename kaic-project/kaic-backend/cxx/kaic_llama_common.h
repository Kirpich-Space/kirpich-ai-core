#pragma once

#ifdef __cplusplus
extern "C" {
#endif

int kaic_llama_common_schema_to_gbnf(const char *schema_json, char **out_gbnf) noexcept;
int kaic_llama_common_parse_chat(const char *text, const char *tools_json, char **out_json) noexcept;
int kaic_llama_common_prepare_chat(
    const char *chat_template,
    const char *messages_json,
    const char *tools_json,
    char **out_json) noexcept;
int kaic_llama_common_parse_prepared_chat(
    const char *chat_template,
    const char *messages_json,
    const char *tools_json,
    const char *text,
    char **out_json) noexcept;
void kaic_llama_common_free_string(char *value) noexcept;

#ifdef __cplusplus
}
#endif
