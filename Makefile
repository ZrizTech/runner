.PHONY: sync-contract

# Maintainers only: copies the vendored wire protocol from the upstream contract repo.
CONTRACT_DIR ?= ../../zriz-contract

sync-contract:
	cp $(CONTRACT_DIR)/contract/*.json contract/
	mkdir -p contract/log contract/fixtures contract/cause
	cp $(CONTRACT_DIR)/cause/reasons.json contract/cause/
	cp $(CONTRACT_DIR)/log/cases.json $(CONTRACT_DIR)/log/lists.json $(CONTRACT_DIR)/log/line.regex contract/log/
	rm -rf contract/fixtures/frames
	cp -R $(CONTRACT_DIR)/fixtures/frames contract/fixtures/frames
