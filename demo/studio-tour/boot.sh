cd demo/studio-tour
# cp .env.example .env && edit it    # uncomment OPENAI_API_KEY=... or similar
set -a; source .env; set +a
cargo run
# # in another shell:
# curl -X POST http://127.0.0.1:4111/demo/seed | jq
# open http://127.0.0.1:4111/studio
