"""Skills API coverage, called from the pinned SDK lifecycle test."""

import io
import zipfile

import openai

from sdk_sessions import rejected


def skill_zip(name, description):
    archive = io.BytesIO()
    with zipfile.ZipFile(archive, "w") as bundle:
        bundle.writestr(
            f"{name}/SKILL.md",
            f"---\nname: {name}\ndescription: {description}\n---\n# {name}\n",
        )
        bundle.writestr(f"{name}/notes.txt", "notes")
    return archive.getvalue()


def check_skills(client):
    skills = client.skills
    # A single ZIP, passed in a list: the pinned SDK sends a lone file that is
    # not in a list as a JSON body.
    skill = skills.create(
        files=[("sdk-skill.zip", skill_zip("sdk-skill", "First."), "application/zip")]
    )
    assert (skill.name, skill.description) == ("sdk-skill", "First.")
    assert (skill.default_version, skill.latest_version) == ("1", "1")
    assert skills.retrieve(skill.id) == skill
    # A directory upload names each file by its path.
    directory = skills.create(
        files=[
            ("sdk-notes/SKILL.md", b"---\nname: sdk-notes\ndescription: Notes.\n---\n"),
            ("sdk-notes/a.txt", b"a"),
        ]
    )
    listed = [item.id for item in skills.list(order="asc")]
    assert listed[-2:] == [skill.id, directory.id]
    assert [item.id for item in skills.list(limit=1).data] == [directory.id]

    with zipfile.ZipFile(io.BytesIO(skills.content.retrieve(skill.id).read())) as bundle:
        assert sorted(bundle.namelist()) == ["sdk-skill/SKILL.md", "sdk-skill/notes.txt"]

    versions = skills.versions
    second = versions.create(
        skill.id,
        files=[("v2.zip", skill_zip("sdk-skill", "Second."), "application/zip")],
    )
    assert (second.skill_id, second.version, second.description) == (
        skill.id,
        "2",
        "Second.",
    )
    assert skills.retrieve(skill.id).default_version == "1"
    third = versions.create(
        skill.id,
        files=[("v3.zip", skill_zip("sdk-skill", "Third."), "application/zip")],
        default=True,
    )
    assert skills.retrieve(skill.id).description == "Third."
    assert versions.retrieve("2", skill_id=skill.id) == second
    assert [item.version for item in versions.list(skill.id)] == ["3", "2", "1"]
    page = versions.list(skill.id, order="asc", limit=2)
    assert ([item.version for item in page.data], page.has_more) == (["1", "2"], True)
    contents = versions.content.retrieve("2", skill_id=skill.id).read()
    assert zipfile.ZipFile(io.BytesIO(contents)).read("sdk-skill/SKILL.md").startswith(b"---")

    updated = skills.update(skill.id, default_version="2")
    assert (updated.default_version, updated.latest_version) == ("2", "3")
    rejected(openai.BadRequestError, skills.update, skill.id, default_version="9")
    rejected(openai.BadRequestError, versions.delete, "2", skill_id=skill.id)
    deleted = versions.delete(third.version, skill_id=skill.id)
    assert deleted.model_dump() == {
        "id": third.id,
        "object": "skill.version.deleted",
        "deleted": True,
        "version": "3",
    }
    rejected(openai.NotFoundError, versions.retrieve, "3", skill_id=skill.id)
    rejected(
        openai.BadRequestError,
        skills.create,
        files=[("broken.zip", b"not a zip", "application/zip")],
    )

    for created in [skill, directory]:
        assert skills.delete(created.id).model_dump() == {
            "id": created.id,
            "object": "skill.deleted",
            "deleted": True,
        }
        rejected(openai.NotFoundError, skills.retrieve, created.id)
