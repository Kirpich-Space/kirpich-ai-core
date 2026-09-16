#include "kaic_llama_common.h"

#include "chat.h"
#include "json-schema-to-grammar.h"

#include <cstdlib>
#include <cstring>
#include <string>
#include <utility>

#include <nlohmann/json.hpp>

namespace {

bool copy_string(const std::string &value, char **out) {
    if (out == nullptr) {
        return false;
    }
    *out = static_cast<char *>(std::malloc(value.size() + 1));
    if (*out == nullptr) {
        return false;
    }
    std::memcpy(*out, value.c_str(), value.size() + 1);
    return true;
}

common_chat_params prepare_chat(
    const char *chat_template,
    const char *messages_json,
    const char *tools_json) {
    if (chat_template == nullptr || messages_json == nullptr || tools_json == nullptr) {
        throw std::invalid_argument("null chat input");
    }
    auto templates = common_chat_templates_init(nullptr, chat_template);
    if (!templates) {
        throw std::runtime_error("chat template initialization failed");
    }

    common_chat_templates_inputs inputs;
    inputs.messages = common_chat_msgs_parse_oaicompat(
        nlohmann::ordered_json::parse(messages_json));
    inputs.tools = common_chat_tools_parse_oaicompat(
        nlohmann::ordered_json::parse(tools_json));
    inputs.add_generation_prompt = true;
    inputs.use_jinja = true;
    inputs.tool_choice = COMMON_CHAT_TOOL_CHOICE_AUTO;
    inputs.parallel_tool_calls = false;
    inputs.reasoning_format = COMMON_REASONING_FORMAT_NONE;
    inputs.enable_thinking = false;
    return common_chat_templates_apply(templates.get(), inputs);
}

nlohmann::ordered_json params_to_json(const common_chat_params &params) {
    nlohmann::ordered_json triggers = nlohmann::ordered_json::array();
    for (const auto &trigger : params.grammar_triggers) {
        triggers.push_back({
            {"type", static_cast<int>(trigger.type)},
            {"value", trigger.value},
            {"token", trigger.token},
        });
    }
    return {
        {"prompt", params.prompt},
        {"grammar", params.grammar},
        {"grammar_lazy", params.grammar_lazy},
        {"generation_prompt", params.generation_prompt},
        {"grammar_triggers", std::move(triggers)},
        {"preserved_tokens", params.preserved_tokens},
        {"additional_stops", params.additional_stops},
    };
}

} // namespace

extern "C" int kaic_llama_common_schema_to_gbnf(
    const char *schema_json,
    char **out_gbnf) noexcept {
    try {
        if (schema_json == nullptr) {
            return 2;
        }
        const auto schema = nlohmann::ordered_json::parse(schema_json);
        const auto gbnf = json_schema_to_grammar(schema);
        return copy_string(gbnf, out_gbnf) ? 0 : 3;
    } catch (...) {
        return 4;
    }
}

extern "C" int kaic_llama_common_parse_chat(
    const char *text,
    const char *tools_json,
    char **out_json) noexcept {
    try {
        if (text == nullptr || tools_json == nullptr) {
            return 2;
        }
        common_chat_parser_params parser_params;
        parser_params.parse_tool_calls = true;
        parser_params.reasoning_format = COMMON_REASONING_FORMAT_NONE;
        const auto tools = nlohmann::ordered_json::parse(tools_json);
        (void) common_chat_tools_parse_oaicompat(tools);
        const auto message = common_chat_parse(text, false, parser_params);
        const auto json = message.to_json_oaicompat().dump();
        return copy_string(json, out_json) ? 0 : 3;
    } catch (...) {
        return 4;
    }
}

extern "C" int kaic_llama_common_prepare_chat(
    const char *chat_template,
    const char *messages_json,
    const char *tools_json,
    char **out_json) noexcept {
    try {
        const auto params = prepare_chat(chat_template, messages_json, tools_json);
        return copy_string(params_to_json(params).dump(), out_json) ? 0 : 3;
    } catch (...) {
        return 4;
    }
}

extern "C" int kaic_llama_common_parse_prepared_chat(
    const char *chat_template,
    const char *messages_json,
    const char *tools_json,
    const char *text,
    char **out_json) noexcept {
    try {
        if (text == nullptr) {
            return 2;
        }
        auto params = prepare_chat(chat_template, messages_json, tools_json);
        common_chat_parser_params parser_params(params);
        parser_params.parser.load(params.parser);
        parser_params.parse_tool_calls = true;
        parser_params.reasoning_format = COMMON_REASONING_FORMAT_NONE;
        const auto message = common_chat_parse(text, false, parser_params);
        return copy_string(message.to_json_oaicompat().dump(), out_json) ? 0 : 3;
    } catch (...) {
        return 4;
    }
}

extern "C" void kaic_llama_common_free_string(char *value) noexcept {
    std::free(value);
}
