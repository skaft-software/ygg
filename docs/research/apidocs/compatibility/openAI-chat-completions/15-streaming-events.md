# Chat Completions streaming events

Streaming events

Stream Chat Completions in real time. Receive chunks of completions returned from the model using server-sent events. Learn more.

chat.completion.chunk

Represents a streamed chunk of a chat completion response returned by the model, based on the provided input. Learn more.

id: string

A unique identifier for the chat completion. Each chunk has the same ID.

choices: array of { delta, finish_reason, index, logprobs }

A list of chat completion choices. Can contain more than one elements if n is greater than 1. Can also be empty for the last chunk if you set stream_options: {"include_usage": true}.

created: number

The Unix timestamp (in seconds) of when the chat completion was created. Each chunk has the same timestamp.

formatunixtime
model: string

The model to generate the completion.

object: "chat.completion.chunk"

The object type, which is always chat.completion.chunk.

moderation: optional { input, output }

Moderation results for the request input and generated output. Present on the moderation chunk when moderated completions are requested.

service_tier: optional "auto" or "default" or "flex" or 2 more

Specifies the processing type used for serving the request.

If set to 'auto', then the request will be processed with the service tier configured in the Project settings. Unless otherwise configured, the Project will use 'default'.
If set to 'default', then the request will be processed with the standard pricing and performance for the selected model.
If set to 'flex' or 'priority', then the request will be processed with the corresponding service tier.
When not set, the default behavior is 'auto'.

When the service_tier parameter is set, the response body will include the service_tier value based on the processing mode actually used to serve the request. This response value may be different from the value set in the parameter.

Deprecatedsystem_fingerprint: optional string

This fingerprint represents the backend configuration that the model runs with. Can be used in conjunction with the seed request parameter to understand when backend changes have been made that might impact determinism.

usage: optional CompletionUsage { completion_tokens, prompt_tokens, total_tokens, 2 more }

An optional field that will only be present when you set stream_options: {"include_usage": true} in your request. When present, it contains a null value except for the last chunk which contains the token usage statistics for the entire request.

NOTE: If the stream is interrupted or cancelled, you may not receive the final usage chunk which contains the total token usage for the request.

chat.completion.chunk
{"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4o-mini", "system_fingerprint": "fp_44709d6fcb", "choices":[{"index":0,"delta":{"role":"assistant","content":""},"logprobs":null,"finish_reason":null}]}

{"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4o-mini", "system_fingerprint": "fp_44709d6fcb", "choices":[{"index":0,"delta":{"content":"Hello"},"logprobs":null,"finish_reason":null}]}

....

{"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4o-mini", "system_fingerprint": "fp_44709d6fcb", "choices":[{"index":0,"delta":{},"logprobs":null,"finish_reason":"stop"}]}