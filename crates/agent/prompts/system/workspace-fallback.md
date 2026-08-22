Workspace
Directory: {{ cwd }}{% if vcs %}
Repository: {{ vcs.root }}
Revision: {{ vcs.head }}{% endif %}