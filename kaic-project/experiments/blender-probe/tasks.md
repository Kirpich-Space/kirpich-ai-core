# Задания пробы — зафиксированы ДО первого обращения к модели

Пять заданий возрастающей сложности. Формулировки — на естественном языке,
без упоминания имён функций API. Один и тот же текст уходит каждой модели,
меняется только `<OUT>` — путь песочницы.

`<OUT>` = `C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out/<модель>/`

## Язык заданий — английский. Почему

Реальный пользователь KAIC говорит по-русски, и честнее было бы мерить
по-русски. Но тогда провал не удалось бы разделить: не справилась модель с
Blender или с русским. Английский — лучший случай для модели; если она не
справляется в лучшем случае, ответ отрицательный однозначно. Русский
остаётся непроверенной переменной и вынесен в долги.

---

## Задание 1 — тривиальное

```
Create a cube at the world origin. Save the scene as a .blend file at
<OUT>task1.blend
```

## Задание 2 — простое с материалом

```
Create a cube and make it red. Render the scene to a PNG image, 320 by 240
pixels, and save the image at <OUT>task2.png
```

## Задание 3 — составное

```
Put three objects of different kinds in a row along the X axis, each one a
different size. Aim a camera at them and add a light so they are visible.
Render the scene to a PNG image, 320 by 240 pixels, at <OUT>task3.png and
also save the scene as a .blend file at <OUT>task3.blend
```

## Задание 4 — с состоянием

```
Open the existing Blender file at <OUT>task1.blend. Find the object called
Cube and move it 3 units up. Save the result as a new file at
<OUT>task4.blend without changing the original.
```

## Задание 5 — с исправлением

Берётся первое задание, на котором модель споткнулась. Модели возвращается
её собственный скрипт и ДОСЛОВНЫЙ текст ошибки Blender, с просьбой починить:

```
This script failed. Here is the error Blender printed:

<stderr дословно, первые 40 строк>

Fix the script. Reply with the corrected script only.
```

Если модель не споткнулась нигде — условие портится намеренно: у неё
запрашивается операция, которой в API 5.2 нет.

```
Create a cube and apply the "quantum smoothing" modifier to it with
strength 0.5, then save the scene as a .blend file at <OUT>task5.blend
```

Предмет измерения здесь — не успех, а поведение: скажет ли модель, что
такого модификатора нет, или выдумает вызов.
