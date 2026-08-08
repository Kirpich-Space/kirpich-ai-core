

import bpy
from pathlib import Path

output_dir = Path(r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Fable9B_t0.2_en_r3")
output_path = output_dir / "task2.png"
output_dir.mkdir(parents=True, exist_ok=True)

bpy.context.scene.render.resolution_x = 320
bpy.context.scene.render.resolution_y = 240
bpy.context.scene.render.filepath = str(output_path)

cube = bpy.ops.mesh.primitive_cube_add(size=1)
obj = bpy.context.active_object
mat = bpy.data.materials.new('CubeMat')
obj.material_slots[0].material = mat
node_tree = mat.node_tree
node = node_tree.nodes.new('ShaderNodeBsdfPrincipled')
node.inputs['Base Color'].default_value = (1.0, 0.0, 0.0, 1.0)

bpy.ops.render.render(write_still=True)