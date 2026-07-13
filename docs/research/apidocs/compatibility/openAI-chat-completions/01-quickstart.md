# Developer quickstart

The OpenAI API provides a simple interface to state-of-the-art AI [models](https://developers.openai.com/api/docs/models) for text generation, natural language processing, computer vision, and more. Get started by creating an API Key and running your first API call. Discover how to generate text, analyze images, build agents, and more.

## Create and export an API key



StatsigClient.logEvent("quickstart_create_api_key_click", null, null)
  }
>
  Create an API Key


<p></p>
Before you begin, create an API key in the dashboard, which you'll use to
securely [access the API](https://developers.openai.com/api/docs/api-reference/authentication). Store the key
in a safe location, like a [`.zshrc`
file](https://www.freecodecamp.org/news/how-do-zsh-configuration-files-work/) or
another text file on your computer. Once you've generated an API key, export it
as an [environment variable](https://en.wikipedia.org/wiki/Environment_variable)
in your terminal.



<div data-content-switcher-pane data-value="macOS">
    <div class="hidden">macOS / Linux</div>
    </div>
  <div data-content-switcher-pane data-value="windows" hidden>
    <div class="hidden">Windows</div>
    </div>



OpenAI SDKs are configured to automatically read your API key from the system environment.

## Install the OpenAI SDK and Run an API Call



<div data-content-switcher-pane data-value="javascript">
    <div class="hidden">JavaScript</div>
    </div>
  <div data-content-switcher-pane data-value="python" hidden>
    <div class="hidden">Python</div>
    </div>
  <div data-content-switcher-pane data-value="csharp" hidden>
    <div class="hidden">.NET</div>
    </div>
  <div data-content-switcher-pane data-value="java" hidden>
    <div class="hidden">Java</div>
    </div>
  <div data-content-switcher-pane data-value="golang" hidden>
    <div class="hidden">Go</div>
    </div>


<a
  href="https://github.com/openai/openai-responses-starter-app"
  target="_blank"
  rel="noreferrer"
>
  

<span slot="icon">
      </span>
    Start building with the Responses API.


</a>

[

<span slot="icon">
      </span>
    Learn more about prompting, message roles, and building conversational apps.

](https://developers.openai.com/api/docs/guides/text)

## Add credits to keep building



StatsigClient.logEvent("quickstart_add_credits_billing_click", null, null)
  }
>
  Go to billing


{/* prettier-ignore */}
<div className="mt-2">Congrats on running a free test API request! Start building real applications with higher limits and use <a href="/api/docs/models" target="_blank">our models</a> to generate text, audio, images, videos and more.</div>

<div className="mt-2">
  Explore tools and docs designed to help you ship faster:
</div>
<a
  href="https://platform.openai.com/chat"
  target="_blank"
  rel="noreferrer"
  onClick={() =>
    StatsigClient.logEvent(
      "quickstart_add_credits_chat_playground_click",
      null,
      null
    )
  }
>
  

<span slot="icon">
      </span>
    Build & test conversational prompts and embed them in your app.


</a>
[

<span slot="icon">
      </span>
    Use the Agents SDK to build, run, and observe agent workflows.

](https://developers.openai.com/api/docs/guides/agents)

## Analyze images and files

Send image URLs, uploaded files, or PDF documents directly to the model to extract text, classify content, or detect visual elements.



<div data-content-switcher-pane data-value="image-url">
    <div class="hidden">Image URL</div>
    </div>
  <div data-content-switcher-pane data-value="file-url" hidden>
    <div class="hidden">File URL</div>
    </div>
  <div data-content-switcher-pane data-value="file-upload" hidden>
    <div class="hidden">Upload file</div>
    </div>



[

<span slot="icon">
      </span>
    Learn to use image inputs to the model and extract meaning from images.

](https://developers.openai.com/api/docs/guides/images)

[

<span slot="icon">
      </span>
    Learn to use file inputs to the model and extract meaning from documents.

](https://developers.openai.com/api/docs/guides/file-inputs)

## Extend the model with tools

Give the model access to external data and functions by attaching [tools](https://developers.openai.com/api/docs/guides/tools). Use built-in tools like web search or file search, or define your own for calling APIs, running code, or integrating with third-party systems.



<div data-content-switcher-pane data-value="web-search">
    <div class="hidden">Web search</div>
    </div>
  <div data-content-switcher-pane data-value="file-search" hidden>
    <div class="hidden">File search</div>
    </div>
  <div data-content-switcher-pane data-value="code-interpreter" hidden>
    <div class="hidden">Code Interpreter</div>
    </div>
  <div data-content-switcher-pane data-value="function-calling" hidden>
    <div class="hidden">Function calling</div>
    </div>
  <div data-content-switcher-pane data-value="remote-mcp" hidden>
    <div class="hidden">Remote MCP</div>
    </div>



[

<span slot="icon">
      </span>
    Learn about powerful built-in tools like web search and file search.

](https://developers.openai.com/api/docs/guides/tools)

[

<span slot="icon">
      </span>
    Learn to enable the model to call your own custom code.

](https://developers.openai.com/api/docs/guides/function-calling)

## Stream responses and build realtime apps

Use server‑sent [streaming events](https://developers.openai.com/api/docs/guides/streaming-responses) to show results as they’re generated, or the [Realtime API](https://developers.openai.com/api/docs/guides/realtime) for interactive voice and multimodal apps.

[

<span slot="icon">
      </span>
    Use server-sent events to stream model responses to users fast.

](https://developers.openai.com/api/docs/guides/streaming-responses)

[

<span slot="icon">
      </span>
    Use WebRTC or WebSockets for super fast speech-to-speech AI apps.

](https://developers.openai.com/api/docs/guides/realtime)

## Build agents

Use the OpenAI platform to build [agents](https://developers.openai.com/api/docs/guides/agents) capable of taking action—like [controlling computers](https://developers.openai.com/api/docs/guides/tools-computer-use)—on behalf of your users. Use the [Agents SDK](https://developers.openai.com/api/docs/guides/agents) to create orchestration logic on the backend.

[

<span slot="icon">
      </span>
    Learn how to use the OpenAI platform to build powerful, capable AI agents.

](https://developers.openai.com/api/docs/guides/agents)