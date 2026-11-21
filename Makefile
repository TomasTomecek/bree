# Build the RPM (README, "Building the RPM"). `make srpm` writes the source
# tarball, the vendor tarball and the SRPM into rpm/; `make rpm` rebuilds that
# SRPM in mock. The build is offline: the crates come from the vendor tarball.
MOCK_CONFIG ?= fedora-44-x86_64
# Packit sets PACKIT_PROJECT_VERSION to the version it builds (a snapshot
# version between releases); the tarballs and the spec must agree with it.
VERSION := $(or $(PACKIT_PROJECT_VERSION),$(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1))
NAME := bree-$(VERSION)
SRPM := rpm/$(NAME)-1.src.rpm
# Where `archive` writes the two tarballs and the filled-in spec. Packit's
# create-archive action (.packit.yaml) writes next to bree.spec, in place.
OUTDIR ?= rpm
SPEC_OUT ?= rpm/bree.spec

.PHONY: archive version srpm rpm clean-rpm
# The source tarball (git archive of HEAD), the vendor tarball and bree.spec
# with its version and commit globals set. The last line printed is the source
# tarball's path, which Packit reads (run it as `make -s archive`).
archive:
	test -n "$(VERSION)"
	mkdir -p $(OUTDIR) $(dir $(SPEC_OUT))
	mkdir $(OUTDIR)/vendor-tmp
	git archive --format=tar.gz --prefix=$(NAME)/ -o $(OUTDIR)/$(NAME).tar.gz HEAD
	cargo vendor --locked $(OUTDIR)/vendor-tmp/vendor >/dev/null
	tar -C $(OUTDIR)/vendor-tmp -czf $(OUTDIR)/$(NAME)-vendor.tar.gz vendor
	rm -r $(OUTDIR)/vendor-tmp
	sed -e 's/^\(%global bree_version\) .*/\1 $(VERSION)/' \
		-e "s/^\(%global bree_commit\) .*/\1 $$(git rev-parse --short=12 HEAD)/" \
		bree.spec > $(SPEC_OUT).new
	mv $(SPEC_OUT).new $(SPEC_OUT)
	@echo $(OUTDIR)/$(NAME).tar.gz

# Prints the crate version (Packit's get-current-version action).
version:
	@echo $(VERSION)

srpm:
	rm -rf rpm
	$(MAKE) -s archive >/dev/null
	rpmbuild -bs rpm/bree.spec --define '_sourcedir $(CURDIR)/rpm' \
		--define '_srcrpmdir $(CURDIR)/rpm' --define 'dist %{nil}'
	ls $(SRPM)

rpm: $(SRPM)
	mock -r $(MOCK_CONFIG) --rebuild $(SRPM)
