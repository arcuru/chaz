# Sourced by run.sh: reuse its Synapse, identities, peers and cleanup.
STRANGER_TOKEN="$(mx POST /_matrix/client/v3/login "" "$(jq -nc \
	--arg u stranger --arg p "$STRANGER_PASSWORD" \
	'{type:"m.login.password",identifier:{type:"m.id.user",user:$u},password:$p}')" |
	jq -r '.access_token // empty')"
[[ -n $STRANGER_TOKEN ]] || fail "stranger could not log in"
ROOM_URI="$(jq -rn --arg r "$ROOM_ID" '$r|@uri')"
mx POST "/_matrix/client/v3/rooms/$ROOM_URI/invite" "$PUPPET_TOKEN" \
	"$(jq -nc --arg a "$STRANGER_MXID" '{user_id:$a}')" >/dev/null
mx POST "/_matrix/client/v3/rooms/$ROOM_URI/join" "$STRANGER_TOKEN" '{}' >/dev/null

smoke_send() {
	mx PUT "/_matrix/client/v3/rooms/$ROOM_URI/send/m.room.message/e2e-$(date +%s%N)" \
		"$1" "$(jq -nc --arg a "$AGENT_MXID" --arg b "$2" \
			'{msgtype:"m.text",body:$b,"m.mentions":{user_ids:[$a]}}')" >"$WORKSPACE/smoke-send.json"
	jq -e '.event_id' "$WORKSPACE/smoke-send.json" >/dev/null || fail "Matrix send failed"
}
smoke_evidence() {
	mx GET "/_matrix/client/v3/rooms/$ROOM_URI/messages?dir=f&limit=200" "$PUPPET_TOKEN" >"$WORKSPACE/room.json"
	uv run --no-project "$HERE/real_evidence.py" "$WORKSPACE" "$MARKER" "$@" >"$WORKSPACE/evidence.json"
}

# Deliberately distinguish explicit send from its later local terminal final.
smoke_send "$PUPPET_TOKEN" "Use matrix__send exactly once with body $MARKER-SEND to this room. Then finish with the text $MARKER-LOCAL, without any other tool calls or sends."
wait_for "real explicit send, local final and durable completion" "$REPLY_TIMEOUT" smoke_evidence
cp "$WORKSPACE/evidence.json" "$MATRIX_E2E_EVIDENCE/explicit.json"

# The completed allowed turn is the barrier: exactly two durable attempts and
# four model responses must exist, and no forbidden Matrix event may appear.
smoke_send "$STRANGER_TOKEN" "Use matrix__send to post $MARKER-FORBIDDEN."
smoke_send "$PUPPET_TOKEN" "Use matrix__send exactly once with body $MARKER-BARRIER to this room. Then finish with the text $MARKER-DONE, without any other tool calls or sends."
wait_for "real denial barrier and durable completion" "$REPLY_TIMEOUT" smoke_evidence --final
uv run --no-project "$HERE/real_evidence.py" "$WORKSPACE" "$MARKER" --final >"$MATRIX_E2E_EVIDENCE/result.json"
printf '\033[1;32mPASS\033[0m — real-model smoke: two explicit sends, two local finals, stranger denied, durable records and spend correlated\n' >&2
